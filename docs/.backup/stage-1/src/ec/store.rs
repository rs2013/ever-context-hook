//! JSONL 存储层：会话树的读/写/定位/回溯。
//!
//! ## 文件布局
//!
//! ```
//! <data_root>/sessions/<session_id>.jsonl      # 会话消息树（一行一条 MessageRecord）
//! <data_root>/sessions/<session_id>.leaf       # 当前最新叶子的 message_id
//! ```
//!
//! ## 核心操作
//!
//! - `load_chain(session_id, leaf_id)`：从 leaf 沿 parent_id 上溯到根，得到祖先链。
//! - `locate(session_id, messages)`：从请求尾部在 JSONL 中定位当前叶子。
//! - `append(session_id, records)`：追加新消息。
//! - `update_leaf(session_id, leaf_id)`：更新叶子定位文件。

use crate::ec::identity::{feature_text_of_record, is_projection_message, to_record};
use crate::ec::lock;
use crate::ec::types::{ChatMessage, MessageRecord};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// 存储层错误。
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self { StoreError::Io(e) }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self { StoreError::Json(e) }
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Json(e) => Some(e),
        }
    }
}

/// JSONL 存储。
pub struct Store {
    data_root: PathBuf,
}

impl Store {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self { data_root: data_root.into() }
    }
    /// 数据根字符串（锁路径用）。
    pub fn data_root_str(&self) -> String {
        self.data_root.display().to_string()
    }

    fn sessions_dir(&self) -> PathBuf {
        self.data_root.join("sessions")
    }

    fn session_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.jsonl"))
    }

    /// 会话 JSONL 文件路径（公开，索引定位用）。
    pub fn session_file_path(&self, session_id: &str) -> PathBuf {
        self.session_file(session_id)
    }

    /// 会话 JSONL 文件是否存在。
    pub fn session_exists(&self, session_id: &str) -> bool {
        self.session_file(session_id).exists()
    }

    /// 读会话 JSONL，返回 (行号, 记录) 列表（按文件顺序）。
    ///
    /// 行号 1-based，用于索引文档的 start_line/end_line 定位。
    pub fn read_session_with_lines(&self, session_id: &str) -> Result<Vec<(u64, MessageRecord)>, StoreError> {
        let path = self.session_file(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = fs::File::open(&path)?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: MessageRecord = serde_json::from_str(&line)?;
            out.push((i as u64 + 1, rec));
        }
        Ok(out)
    }

    /// 更新某条记录的 ec 字段（写入压缩产物）。
    ///
    /// 内部持 JSONL 写锁（与 append/import 互斥）。全量重写 JSONL。
    pub fn set_ec(&self, session_id: &str, record_id: &str, ec: &crate::ec::types::EcProjection) -> Result<(), StoreError> {
        let d = self.data_root_str();
        let s = session_id.to_string();
        lock::with_jsonl_lock(&d, &s, || {
            self.set_ec_locked(session_id, record_id, ec).map_err(|e| e.to_string())
        })
        .map_err(|e| StoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(())
    }

    fn set_ec_locked(&self, session_id: &str, record_id: &str, ec: &crate::ec::types::EcProjection) -> Result<(), StoreError> {
        let path = self.session_file(session_id);
        if !path.exists() {
            return Ok(());
        }
        let f = fs::File::open(&path)?;
        let reader = BufReader::new(f);
        let mut lines: Vec<String> = Vec::new();
        let mut found = false;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                lines.push(line);
                continue;
            }
            let mut rec: MessageRecord = serde_json::from_str(&line)?;
            if rec.id == record_id {
                rec.ec = Some(ec.clone());
                found = true;
            }
            lines.push(serde_json::to_string(&rec)?);
        }
        if !found {
            return Ok(()); // 未找到目标记录：静默（可能已被别处更新）
        }
        // 回写（临时文件 + rename）
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut w = fs::File::create(&tmp)?;
            for l in &lines {
                w.write_all(l.as_bytes())?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn leaf_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.leaf"))
    }

    /// 读整个会话 JSONL，返回 id → record 的映射（保序由链回溯保证）。
    pub fn read_session(&self, session_id: &str) -> Result<HashMap<String, MessageRecord>, StoreError> {
        let path = self.session_file(session_id);
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let f = fs::File::open(&path)?;
        let reader = BufReader::new(f);
        let mut map = HashMap::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            let rec: MessageRecord = serde_json::from_str(&line)?;
            map.insert(rec.id.clone(), rec);
        }
        Ok(map)
    }

    /// 从 leaf 沿 parent_id 上溯到根，返回祖先链（根在前，叶在后）。
    pub fn load_chain(
        &self,
        session_id: &str,
        leaf_id: &str,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let map = self.read_session(session_id)?;
        let mut chain = Vec::new();
        let mut cur = Some(leaf_id.to_string());
        // 防环（理论上树无环，但数据损坏时保护）。
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = cur {
            if !visited.insert(id.clone()) {
                break; // 环检测
            }
            match map.get(&id) {
                Some(rec) => {
                    chain.push(rec.clone());
                    cur = rec.parent_id.clone();
                }
                None => break, // 断链：停止（诚实降级）
            }
        }
        chain.reverse(); // 根在前
        Ok(chain)
    }

    /// 读取叶子定位文件。
    pub fn read_leaf(&self, session_id: &str) -> Option<String> {
        let path = self.leaf_file(session_id);
        fs::read_to_string(&path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// 更新叶子定位文件。
    pub fn update_leaf(&self, session_id: &str, leaf_id: &str) -> Result<(), StoreError> {
        fs::create_dir_all(self.sessions_dir())?;
        let path = self.leaf_file(session_id);
        // 原子写：先写临时文件再 rename（避免写一半崩溃留下脏数据）。
        let tmp = path.with_extension("leaf.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(leaf_id.as_bytes())?;
            f.write_all(b"\n")?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 追加消息记录到 JSONL（幂等：按 id 去重）。
    ///
    /// 返回实际新写入的记录数。
    pub fn append(&self, session_id: &str, records: &[MessageRecord]) -> Result<usize, StoreError> {
        if records.is_empty() {
            return Ok(0);
        }
        fs::create_dir_all(self.sessions_dir())?;
        let path = self.session_file(session_id);

        // 读已有 id 集合去重
        let existing = self.read_session(session_id)?;
        let mut written = 0;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        for rec in records {
            if existing.contains_key(&rec.id) {
                continue; // 已存在，跳过
            }
            let line = serde_json::to_string(rec)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            written += 1;
        }
        f.flush()?;
        Ok(written)
    }

    /// 从 OpenAI messages 数组创建完整 JSONL 树（纯规则，不调 LLM）。
    ///
    /// 用于「新对话首次遇到」：首条 user 找不到对应 JSONL 时，把请求里的全部
    /// 原生消息逐条转成 MessageRecord（message_id + parent_id 链），并更新叶子。
    ///
    /// 跳过投影消息（ec_meta / ec_management / ec_recall_result）——它们不是原生对话。
    /// 返回导入的记录数（幂等：已存在的记录跳过）。
    pub fn import_messages(&self, session_id: &str, messages: &[ChatMessage]) -> Result<usize, StoreError> {
        // 逐条转换，维护 parent 链
        let mut records: Vec<MessageRecord> = Vec::new();
        let mut parent: Option<String> = None;
        for m in messages {
            if is_projection_message(m) {
                continue; // EC 注入的消息不入 JSONL
            }
            let rec = to_record(m, parent.as_deref());
            parent = Some(rec.id.clone());
            records.push(rec);
        }
        if records.is_empty() {
            return Ok(0);
        }
        // 追加 + 更新叶子
        let n = self.append(session_id, &records)?;
        if let Some(last) = records.last() {
            self.update_leaf(session_id, &last.id)?;
        }
        Ok(n)
    }

    /// 从请求尾部定位当前叶子。
    ///
    /// 返回 (leaf_id)。定位失败返回 None。
    /// `tail_features` 按**旧→新**顺序（最后一条 = 当前叶子）。
    /// 规则：找最后一条 feature 的候选 id，逐个沿 parent 上溯验证整段窗口。
    pub fn locate(
        &self,
        session_id: &str,
        tail_features: &[String],
    ) -> Result<Option<String>, StoreError> {
        if tail_features.is_empty() {
            return Ok(None);
        }
        let map = self.read_session(session_id)?;
        if map.is_empty() {
            return Ok(None);
        }
        // 建 feature_text → ids 的倒排
        let mut ft_to_ids: HashMap<String, Vec<String>> = HashMap::new();
        for (id, rec) in &map {
            let ft = feature_text_of_record(rec);
            ft_to_ids.entry(ft).or_default().push(id.clone());
        }

        // 从长窗口到短窗口（越长越精确），窗口最末一条 = 当前叶子
        for n in (1..=tail_features.len()).rev() {
            let window = &tail_features[tail_features.len() - n..]; // 旧→新，长度 n
            let leaf_ft = window.last().unwrap(); // 最末一条 = 叶子
            if let Some(candidates) = ft_to_ids.get(leaf_ft) {
                for leaf_id in candidates {
                    // 从叶子沿 parent 上溯，窗口反向（新→旧）验证
                    if self.chain_matches_up(&map, leaf_id, window) {
                        return Ok(Some(leaf_id.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// 从叶子 id 出发沿 parent_id 上溯，验证窗口（旧→新）是否连续匹配。
    /// 从窗口最末（=叶子）开始反向比对到窗口最前。
    fn chain_matches_up(&self, map: &HashMap<String, MessageRecord>, leaf_id: &str, window: &[String]) -> bool {
        let mut cur = Some(leaf_id.to_string());
        // 反向遍历 window：从最末（叶子）到最前（祖先）
        for ft in window.iter().rev() {
            match &cur {
                Some(id) => match map.get(id) {
                    Some(rec) => {
                        if feature_text_of_record(rec) != *ft {
                            return false;
                        }
                        cur = rec.parent_id.clone();
                    }
                    None => return false,
                },
                None => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::types::MessageRecord;

    fn tmp_store() -> (Store, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ec-store-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        (Store::new(dir.clone()), dir)
    }

    fn rec(id: &str, parent: Option<&str>, role: &str, content: &str) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: None,
        }
    }

    #[test]
    fn append_and_load_chain() {
        let (store, dir) = tmp_store();
        let records = vec![
            rec("root", None, "user", "hello"),
            rec("a1", Some("root"), "assistant", "hi"),
            rec("u2", Some("a1"), "user", "继续"),
        ];
        assert_eq!(store.append("s1", &records).unwrap(), 3);
        // 幂等：重复 append 不重复写
        assert_eq!(store.append("s1", &records).unwrap(), 0);

        let chain = store.load_chain("s1", "u2").unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, "root");
        assert_eq!(chain[2].id, "u2");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaf_roundtrip() {
        let (store, dir) = tmp_store();
        assert_eq!(store.read_leaf("s1"), None);
        store.update_leaf("s1", "u2").unwrap();
        assert_eq!(store.read_leaf("s1"), Some("u2".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn locate_single_match() {
        let (store, dir) = tmp_store();
        let records = vec![
            rec("root", None, "user", "hello"),
            rec("a1", Some("root"), "assistant", "hi"),
        ];
        store.append("s1", &records).unwrap();

        // 尾部最后一条是 a1，feature = "assistant:hi"
        let located = store.locate("s1", &["assistant:hi".to_string()]).unwrap();
        assert_eq!(located, Some("a1".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn locate_sequence_disambiguates() {
        let (store, dir) = tmp_store();
        let records = vec![
            rec("root", None, "user", "hello"),
            rec("a1", Some("root"), "assistant", "hi"),
            rec("u2", Some("a1"), "user", "继续"),
            rec("a3", Some("u2"), "assistant", "hi"),  // 与 a1 内容重复！
            rec("u4", Some("a3"), "user", "继续"),      // 与 u2 内容重复！
        ];
        store.append("s1", &records).unwrap();

        // 尾部序列：["user:继续", "assistant:hi", "user:继续"]（新→旧）
        // 单条 "user:继续" 有歧义（u2 和 u4），但序列匹配能定位到 u4
        let tail = vec![
            "user:继续".to_string(),
            "assistant:hi".to_string(),
            "user:继续".to_string(),
        ];
        let located = store.locate("s1", &tail).unwrap();
        assert_eq!(located, Some("u4".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }
}
