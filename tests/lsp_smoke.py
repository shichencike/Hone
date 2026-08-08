# -*- coding: utf-8 -*-
"""hone lsp 冒烟测试：模拟 LSP 客户端会话（initialize / didOpen / completion / shutdown）"""
import json
import subprocess
import sys

ZAP = sys.argv[1] if len(sys.argv) > 1 else "./target/debug/hone"

proc = subprocess.Popen([ZAP, "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)


def send(msg):
    body = json.dumps(msg, ensure_ascii=False).encode("utf-8")
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    proc.stdin.flush()


def recv():
    headers = {}
    while True:
        line = proc.stdout.readline()
        if line == b"":
            return None
        line = line.decode().strip()
        if line == "":
            break
        k, _, v = line.partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers["content-length"])
    return json.loads(proc.stdout.read(n).decode("utf-8"))


# 1. initialize
send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
      "params": {"capabilities": {}}})
r = recv()
assert r["id"] == 1 and "capabilities" in r["result"], r
print("initialize        OK  capabilities:", sorted(r["result"]["capabilities"].keys()))

send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

# 2. didOpen（含错误：x 类型锁定）
send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
    "textDocument": {"uri": "file:///demo.hn", "languageId": "hone",
                     "version": 1, "text": "x = 10;\nx = \"Hone\";\n"}}})
diag = recv()
assert diag["method"] == "textDocument/publishDiagnostics", diag
d = diag["params"]["diagnostics"][0]
assert d["code"] == "H001", d
assert d["range"]["start"]["line"] == 1, d  # 第二行（0-based）
print("诊断(didOpen)    OK  H001 @ line", d["range"]["start"]["line"], "->", d["message"][:40])

# 3. completion
send({"jsonrpc": "2.0", "id": 2, "method": "textDocument/completion",
      "params": {"textDocument": {"uri": "file:///demo.hn"}, "position": {"line": 0, "character": 0}}})
r = recv()
items = r["result"]["items"]
assert any(i["label"] == "print" for i in items), r
assert any(i["label"] == "time.now" for i in items), r
print("completion        OK  补全项", len(items), "个（含 print / time.now）")

# 4. hover
send({"jsonrpc": "2.0", "id": 3, "method": "textDocument/hover",
      "params": {"textDocument": {"uri": "file:///demo.hn"}, "position": {"line": 0, "character": 0}}})
r = recv()
assert "Hone" in r["result"]["contents"]["value"], r
print("hover             OK")

# 5. shutdown / exit
send({"jsonrpc": "2.0", "id": 4, "method": "shutdown", "params": {}})
assert recv()["result"] is None
send({"jsonrpc": "2.0", "method": "exit", "params": {}})
proc.wait(timeout=5)
print("shutdown/exit     OK  exit code", proc.returncode)
print("=== LSP 冒烟测试全部通过 ===")
