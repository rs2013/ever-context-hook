#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 Pi JSONL 长对话喂给 ec-hook 的 request.prepare，并把「投影后的请求」完整落盘，
供事后复盘 EC 对旧对话的投影结果。

用法: dump-prepare.py <pi.jsonl> <ever-context-hook.exe> <data_dir> <out_prefix>

产出：
  <out_prefix>.invoke.json   原始 invoke（完整历史，含全部 tool_result）
  <out_prefix>.response.json prepare 响应原文
  <out_prefix>.projection.txt 人类可读投影：role/name/长度逐条 + 管理区全文
  <out_prefix>.messages.json  投影后的 messages（供重放）
"""
import json, subprocess, sys, os

def extract_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") == "text")
    return ""

def to_openai_messages(src_path):
    """Pi JSONL → OpenAI Chat messages（保留 role/content/tool_calls/tool_call_id）。"""
    msgs = []
    with open(src_path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if entry.get("type") != "message":
                continue
            m = entry.get("message", {})
            role = m.get("role", "")
            if role not in ("user", "assistant", "toolResult"):
                continue
            content = m.get("content", "")
            oa = {"role": "tool" if role == "toolResult" else role}
            if isinstance(content, list):
                texts, tool_calls = [], []
                for p in content:
                    if isinstance(p, dict) and p.get("type") == "text":
                        texts.append(p.get("text", ""))
                    elif isinstance(p, dict) and p.get("type") == "toolCall":
                        tool_calls.append({"id": p.get("id", ""), "type": "function",
                                           "function": {"name": p.get("name", ""),
                                                        "arguments": json.dumps(p.get("arguments", ""), ensure_ascii=False)}})
                if texts:
                    oa["content"] = "\n".join(texts)
                if tool_calls:
                    oa["tool_calls"] = tool_calls
            else:
                oa["content"] = content
            if role == "toolResult":
                oa["tool_call_id"] = m.get("toolCallId", "")
            msgs.append(oa)
    return msgs

def openai_to_pi_session(msgs):
    """把 OpenAI messages 还原成 convert-pi-session.py 认得的 pi 风格 JSONL 行，便于同源比对。"""
    # 仅用于统计，不需要

def main():
    src, exe, data_dir, prefix = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
    msgs = to_openai_messages(src)
    total_chars = sum(len(json.dumps(m, ensure_ascii=False)) for m in msgs)
    print(f"消息数: {len(msgs)}, 总字符数: {total_chars}, ~token(字节/3): {total_chars // 3}")

    invoke = {
        "v": 1, "id": "p-sim", "op": "invoke", "method": "request.prepare",
        "params": {
            "request": {"model": "deepseek-flash", "messages": msgs},
            "config": {"enabled": True},
            "hook_id": "ec", "chain": {"index": 0, "total": 1},
        },
    }
    os.makedirs(os.path.dirname(prefix) or ".", exist_ok=True)
    with open(prefix + ".invoke.json", "w", encoding="utf-8") as f:
        json.dump(invoke, f, ensure_ascii=False, indent=1)

    proc = subprocess.run([exe, "call", "--data", data_dir],
                          input=(json.dumps(invoke, ensure_ascii=False) + "\n").encode("utf-8"),
                          capture_output=True, timeout=120)
    out = proc.stdout.decode("utf-8", errors="replace")
    err = proc.stderr.decode("utf-8", errors="replace")
    with open(prefix + ".response.json", "w", encoding="utf-8") as f:
        f.write(out)
    with open(prefix + ".stderr.txt", "w", encoding="utf-8") as f:
        f.write(err)
    print(f"退出码: {proc.returncode}")
    print("stderr:", "\n".join(err.strip().split("\n")[-8:]))

    line = next((l for l in out.splitlines() if '"result"' in l or '"ok"' in l), "")
    resp = json.loads(line) if line else {}
    if not resp.get("ok"):
        print("!! prepare 未成功:", json.dumps(resp, ensure_ascii=False)[:400])
        return
    proj = resp["result"]["request"]["messages"]

    lines = [f"投影消息数: {len(proj)}（原始 {len(msgs)}）", "=" * 80]
    for i, m in enumerate(proj):
        name = m.get("name") or ""
        c = m.get("content", "")
        clen = len(c) if isinstance(c, str) else len(json.dumps(c, ensure_ascii=False))
        role = m.get("role") or ""
        lines.append(f"[{i:3d}] role={role:9s} name={name:18s} len={clen:7d} tool_calls={len(m.get('tool_calls', []) or [])}")
    lines.append("=" * 80)
    # 管理区全文
    for m in proj:
        if m.get("name") == "ec_management":
            lines.append("── 管理区（ec_management）全文 ──")
            lines.append(m.get("content", ""))
            lines.append("── 管理区结束 ──")
    with open(prefix + ".projection.txt", "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    with open(prefix + ".messages.json", "w", encoding="utf-8") as f:
        json.dump(proj, f, ensure_ascii=False, indent=1)
    print(f"已写出: {prefix}.projection.txt / .messages.json / .response.json")

if __name__ == "__main__":
    main()
