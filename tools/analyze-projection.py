#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
分析 ec-hook 投影结果：消息数、角色分布、token 估算、首尾消息、工具链闭合。
"""
import json, subprocess, sys
from collections import Counter

sys.path.insert(0, "tools")

def extract_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") == "text")
    return ""

def to_openai_messages(src_path):
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
            oa = {}
            if isinstance(content, list):
                text_parts, tool_calls = [], []
                for p in content:
                    if isinstance(p, dict) and p.get("type") == "text":
                        text_parts.append(p.get("text", ""))
                    elif isinstance(p, dict) and p.get("type") == "toolCall":
                        tool_calls.append({
                            "id": p.get("id", ""), "type": "function",
                            "function": {"name": p.get("name", ""), "arguments": json.dumps(p.get("arguments", ""), ensure_ascii=False)},
                        })
                if role == "toolResult":
                    oa = {"role": "tool", "content": "\n".join(text_parts), "tool_call_id": m.get("toolCallId", "")}
                elif role == "assistant":
                    oa = {"role": "assistant"}
                    if text_parts:
                        oa["content"] = "\n".join(text_parts)
                    if tool_calls:
                        oa["tool_calls"] = tool_calls
                else:
                    oa = {"role": role, "content": "\n".join(text_parts)}
            else:
                if role == "toolResult":
                    oa = {"role": "tool", "content": content, "tool_call_id": m.get("toolCallId", "")}
                else:
                    oa = {"role": role, "content": content}
            msgs.append(oa)
    return msgs

def main():
    src, exe, data_dir = sys.argv[1], sys.argv[2], sys.argv[3]
    msgs = to_openai_messages(src)
    invoke = {
        "v": 1, "id": "p", "op": "invoke", "method": "request.prepare",
        "params": {"request": {"model": "deepseek-flash", "messages": msgs},
                   "config": {"enabled": True}, "hook_id": "ec", "chain": {"index": 0, "total": 1}},
    }
    proc = subprocess.run([exe, "call", "--data", data_dir],
                          input=json.dumps(invoke, ensure_ascii=False).encode(),
                          capture_output=True, timeout=60)
    out = json.loads(proc.stdout.decode())
    if not out.get("ok"):
        print(f"拒绝: {out.get('error',{})}")
        return
    req = out["result"]["request"]
    pmsgs = req["messages"]
    print(f"投影消息数: {len(pmsgs)}")
    c = Counter(m.get("role") for m in pmsgs)
    print(f"角色分布: {dict(c)}")
    total_chars = sum(len(json.dumps(m, ensure_ascii=False)) for m in pmsgs)
    print(f"投影总字符: {total_chars}, 粗估 token: {total_chars // 3}")
    # 首尾消息
    for label, m in [("首", pmsgs[0]), ("次", pmsgs[1] if len(pmsgs) > 1 else {}), ("末", pmsgs[-1])]:
        role = m.get("role", "?")
        name = m.get("name", "")
        content = m.get("content", "")
        if isinstance(content, str):
            preview = content[:60]
        else:
            preview = str(content)[:60]
        print(f"{label}条: role={role} name={name} content={preview!r}")
    # 检查首条是否是 ec_management / ec_hard_line_notice
    names = [m.get("name") for m in pmsgs]
    print(f"ec_management 在头部: {'ec_management' in names[:3]}")
    print(f"ec_hard_line_notice 在尾部: {'ec_hard_line_notice' in names[-3:]}")
    # 工具链闭合检查：孤儿 tool（tool_call_id 无对应 assistant tool_calls）
    tc_ids = set()
    for m in pmsgs:
        for tc in m.get("tool_calls", []):
            tc_ids.add(tc.get("id", ""))
    orphan = [m.get("tool_call_id") for m in pmsgs if m.get("role") == "tool" and m.get("tool_call_id") not in tc_ids]
    print(f"孤儿 tool 消息数: {len(orphan)}")

if __name__ == "__main__":
    main()
