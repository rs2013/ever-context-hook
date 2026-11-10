//! 进程锁：防并发写 + 过期处理（进程被强杀后锁文件残留）。
//!
//! ## 锁文件格式
//!
//! ```
//! <pid>
//! <unix_timestamp_secs>
//! ```
//!
//! ## 过期策略
//!
//! 1. 锁文件时间戳超过 STALE_SECS（默认 1800s = 30min）→ 视为过期，直接删除重抢。
//! 2. 时间戳未过期但 PID 不存活 → 视为过期（进程已退出但锁未清理）。
//! 3. PID 存活且时间戳未过期 → 拒绝（真的有人在持有）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

/// 锁文件超过此秒数视为过期（进程被强杀后的兜底）。
const STALE_SECS: u64 = 1800; // 30 min

/// 获取 JSONL 写锁路径（短持锁，append/import/set_ec 用）。
pub fn jsonl_lock_path(data_root: &str, session_id: &str) -> PathBuf {
    PathBuf::from(data_root).join("sessions").join(format!("{session_id}.jsonl.lock"))
}

/// 获取 catchup 会话锁路径（持锁到追赶结束）。
pub fn session_lock_path(data_root: &str, session_id: &str) -> PathBuf {
    PathBuf::from(data_root).join("sessions").join(format!("{session_id}.lock"))
}

/// 尝试获取锁（原子 create_new + 过期判断 + PID 存活检查）。
///
/// 返回 `(guard_path, pid, ts)`，调用方负责在释放时删除 guard_path。
pub fn try_acquire(lock_path: &Path) -> Result<(PathBuf, u32, u64), String> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("建锁目录失败: {e}"))?;
    }

    // ① 尝试原子创建
    match fs::OpenOptions::new().write(true).create_new(true).open(lock_path) {
        Ok(mut f) => {
            let pid = std::process::id();
            let ts = now_secs();
            let _ = writeln!(f, "{pid}");
            let _ = writeln!(f, "{ts}");
            return Ok((lock_path.to_path_buf(), pid, ts));
        }
        Err(_) => {} // 已存在：检查是否过期
    }

    // ② 读现有锁信息
    let content = fs::read_to_string(lock_path)
        .map_err(|e| format!("读锁文件失败: {e}"))?;
    let lines: Vec<&str> = content.trim().lines().collect();
    if lines.len() < 2 {
        // 格式损坏：删掉重来
        let _ = fs::remove_file(lock_path);
        return try_acquire(lock_path);
    }
    let pid: u32 = lines[0].parse().unwrap_or(0);
    let ts: u64 = lines[1].parse().unwrap_or(0);

    // ③ 时间戳过期？
    let age = now_secs().saturating_sub(ts);
    if age > STALE_SECS {
        log::warn!("[lock] 锁文件超过 {STALE_SECS}s 未更新（age={age}s），视为过期，删除重试");
        let _ = fs::remove_file(lock_path);
        return try_acquire(lock_path);
    }

    // ④ PID 是否存活？
    if !pid_alive(pid) {
        log::warn!("[lock] PID {pid} 不存活，视为过期，删除重试");
        let _ = fs::remove_file(lock_path);
        return try_acquire(lock_path);
    }

    // ⑤ 锁被持有，拒绝
    Err(format!("锁被 PID {pid} 持有（{age}s 前），拒绝重入"))
}

/// 释放锁（删除锁文件）。
pub fn release(lock_path: &Path) {
    let _ = fs::remove_file(lock_path);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 检查 PID 是否存活（best-effort，跨平台）。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        // Windows: tasklist /fi "PID eq <pid>"
        let out = std::process::Command::new("tasklist")
            .args(["/fi", &format!("PID eq {pid}"), "/nh"])
            .output();
        match out {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => true, // tasklist 不可用：保守不删锁（宁可误拒绝，不误删）
        }
    }
    #[cfg(not(windows))]
    {
        // Unix: kill -0
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

/// JSONL 写操作的便捷封装：获取锁 → 执行 → 释放。
pub fn with_jsonl_lock<F, T>(data_root: &str, session_id: &str, f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    let lock_path = jsonl_lock_path(data_root, session_id);
    let (guard, _pid, _ts) = try_acquire(&lock_path)?;
    let result = f();
    release(&guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_lock() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ec-lock-test-{}", n));
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("test-{}.lock", std::process::id()))
    }

    #[test]
    fn acquire_and_release() {
        let p = tmp_lock();
        let _ = fs::remove_file(&p);
        let (guard, _, _) = try_acquire(&p).unwrap();
        assert!(p.exists());
        release(&guard);
        assert!(!p.exists());
    }

    #[test]
    fn refuses_reentry() {
        let p = tmp_lock();
        let _ = fs::remove_file(&p);
        let (g1, _, _) = try_acquire(&p).unwrap();
        assert!(try_acquire(&p).is_err());
        release(&g1);
    }
}