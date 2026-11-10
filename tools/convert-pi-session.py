#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 Pi JSONL 长对话转换成 ec-hook 的 MessageRecord 格式，模拟「agent 逐轮发送」。

用途：验证 ec-hook 对现有长对话的处理（会话指纹 / JSONL 树 / 管理区投影）。

对齐 ec-hook 的 identity.rs / types.rs：
- session_id = sha256(首条 user 全文) 前 16 hex
- message_id = sha256(feature_text + parent_id) 前 16 hex
- feature_text:
    user        -> "user:" + content
    assistant   -> "assistant:" + content（无工具）或 "assistant:" + "name|args" 连接
    tool_result -> "tool_result:" + tool_call_id + ":" + content
"""
import json, hashlib, sys, os

def sha16(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()[:16]

def extract_text(content) -> str:
    """Pi 的 content 是 string 或 [{"type":"text","text":...}, ...]"""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for p in content:
            if isinstance(p, dict) and p.get("type") == "text":
                parts.append(p.get("text", ""))
            elif isinstance(p, dict) and p.get("type") == "toolCall":
                parts.append(f"[toolCall:{p.get('name','')}]")
        return "\n".join(parts)
    return ""

def feature_text(role: str, content, tool_calls, tool_call_id) -> str:
    if role == "user":
        return "user:" + extract_text(content)
    if role == "assistant":
        if tool_calls:
            calls = "|".join(f"{tc['name']}|{json.dumps(tc.get('arguments',''), ensure_ascii=False)}" for tc in tool_calls)
            return "assistant:" + calls
        return "assistant:" + extract_text(content)
    if role in ("tool", "tool_result", "toolResult"):
        return f"tool_result:{tool_call_id or ''}:{extract_text(content)}"
    return f"{role}:{extract_text(content)}"

def message_id(ft: str, parent_id: str) -> str:
    return sha16(ft + "\x00" + (parent_id or ""))

def convert(src_path: str, dst_dir: str):
    """读 Pi JSONL，转成 ec-hook MessageRecord JSONL + leaf 文件。"""
    os.makedirs(dst_dir, exist_ok=True)

    # 1. 提取消息序列（只取 message 类型的 user/assistant/toolResult）
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
            # 提取 tool calls（assistant 的 content 数组里的 toolCall）
            tool_calls = []
            content = m.get("content", "")
            if isinstance(content, list):
                for p in content:
                    if isinstance(p, dict) and p.get("type") == "toolCall":
                        tool_calls.append({
                            "name": p.get("name", ""),
                            "arguments": p.get("arguments", ""),
                        })
            msgs.append({
                "role": "tool_result" if role == "toolResult" else role,
                "content": extract_text(content),
                "tool_calls": tool_calls,
                "tool_call_id": m.get("toolCallId", ""),
            })

    if not msgs:
        print("无消息", file=sys.stderr)
        return

    # 2. 会话 ID = 首条 user 全文 SHA
    first_user = next((m for m in msgs if m["role"] == "user"), None)
    if not first_user:
        print("无 user 消息", file=sys.stderr)
        return
    session_id = sha16(first_user["content"])
    print(f"会话 ID: {session_id}")
    print(f"消息总数: {len(msgs)}")

    # 3. 逐条生成 MessageRecord（含树结构）
    records = []
    parent_id = None
    for m in msgs:
        ft = feature_text(m["role"], m["content"], m["tool_calls"], m["tool_call_id"])
        mid = message_id(ft, parent_id)
        rec = {
            "id": mid,
            "parent_id": parent_id,
            "role": m["role"],
            "content": m["content"],
        }
        if m["tool_calls"]:
            rec["tool_calls"] = [
                {"id": f"call_{i}", "type": "function",
                 "function": {"name": tc["name"], "arguments": json.dumps(tc["arguments"], ensure_ascii=False) if isinstance(tc["arguments"], dict) else str(tc["arguments"])}}
                for i, tc in enumerate(m["tool_calls"])
            ]
        if m["tool_call_id"]:
            rec["tool_call_id"] = m["tool_call_id"]
            rec["tool_name"] = m["tool_call_id"].split("_")[0] if m["tool_call_id"] else ""
        records.append(rec)
        parent_id = mid

    # 4. 写入 dst_dir/<session_id>.jsonl + .leaf
    dst = os.path.join(dst_dir, f"{session_id}.jsonl")
    with open(dst, "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(os.path.join(dst_dir, f"{session_id}.leaf"), "w", encoding="utf-8") as f:
        f.write(records[-1]["id"] + "\n")

    print(f"已写入: {dst}（{len(records)} 条记录，leaf={records[-1]['id']}）")

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("用法: convert.py <pi.jsonl> <dst_dir>", file=sys.stderr)
        sys.exit(2)
    convert(sys.argv[1], sys.argv[2])
