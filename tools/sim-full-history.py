#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
模拟 agent 发送完整历史：把 Pi JSONL 长对话转成 OpenAI messages，喂给 ec-hook prepare。

验证：ec-hook 收到超长历史时，应触发硬预算门拒绝（prompt_too_long）。
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
            if isinstance(content, list):
                # 提取 text + toolCall
                text_parts = []
                tool_calls = []
                for p in content:
                    if isinstance(p, dict) and p.get("type") == "text":
                        text_parts.append(p.get("text", ""))
                    elif isinstance(p, dict) and p.get("type") == "toolCall":
                        tool_calls.append({
                            "id": p.get("id", ""),
                            "type": "function",
                            "function": {
                                "name": p.get("name", ""),
                                "arguments": json.dumps(p.get("arguments", ""), ensure_ascii=False),
                            },
                        })
                oa = {"role": role if role != "toolResult" else "tool"}
                if text_parts:
                    oa["content"] = "\n".join(text_parts)
                if tool_calls:
                    oa["tool_calls"] = tool_calls
                if role == "toolResult":
                    oa["tool_call_id"] = m.get("toolCallId", "")
            else:
                oa = {"role": role if role != "toolResult" else "tool", "content": content}
                if role == "toolResult":
                    oa["tool_call_id"] = m.get("toolCallId", "")
            msgs.append(oa)
    return msgs

def main():
    src = sys.argv[1]
    exe = sys.argv[2]
    data_dir = sys.argv[3]

    msgs = to_openai_messages(src)
    total_chars = sum(len(json.dumps(m, ensure_ascii=False)) for m in msgs)
    print(f"消息数: {len(msgs)}, 总字符数: {total_chars}")

    # 构造 request.prepare invoke
    invoke = {
        "v": 1, "id": "p-sim", "op": "invoke", "method": "request.prepare",
        "params": {
            "request": {"model": "deepseek-flash", "messages": msgs},
            "config": {"enabled": True},
            "hook_id": "ec", "chain": {"index": 0, "total": 1},
        },
    }
    line = json.dumps(invoke, ensure_ascii=False) + "\n"
    print(f"invoke 大小: {len(line)} 字节")

    # 喂给 ec-hook
    proc = subprocess.run(
        [exe, "call", "--data", data_dir],
        input=line.encode("utf-8"),
        capture_output=True, timeout=60,
    )
    print(f"退出码: {proc.returncode}")
    stdout = proc.stdout.decode("utf-8", errors="replace")
    stderr = proc.stderr.decode("utf-8", errors="replace")
    # 只打印关键信息
    print("=== stdout 关键部分 ===")
    if "prompt_too_long" in stdout:
        print("[拒绝] prompt_too_long 触发 ✅")
        # 提取 message
        import re
        m = re.search(r'"message":"([^"]*)"', stdout)
        if m:
            print(f"  消息: {m.group(1)[:200]}")
    else:
        print(stdout[:500])
    print("=== stderr 末尾 ===")
    for l in stderr.strip().split("\n")[-5:]:
        print(l)

if __name__ == "__main__":
    main()
