# lsp_smoke.py - LSP 端到端冒烟：启动 hone lsp，经 stdio 验证各能力
import json
import subprocess
import sys

URI = "file:///test/demo.hn"
SRC = """// 演示文档
/// 文档注释
fn add(a: int, b: int) -> int {
    return a + b;
}
struct Point { x: int, y: int };
class Box {
    fn area(x) -> int {
        return x * 2;
    }
}
x = 10;
y = add(x, 5);
p = Point(1, 2);
// 行注释
area = match y {
    0 => "zero",
    _ => "other",
};
"""


def msg(m):
    b = json.dumps(m).encode()
    return b"Content-Length: " + str(len(b)).encode() + b"\r\n\r\n" + b


def main():
    p = subprocess.Popen(
        [r"D:\shichencike\Desktop\hone\target\debug\hone.exe", "lsp"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    req_id = [0]

    def send(m):
        p.stdin.write(msg(m))
        p.stdin.flush()

    def req(method, params):
        req_id[0] += 1
        rid = req_id[0]
        send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def read_msg():
        # 读头
        headers = {}
        while True:
            line = p.stdout.readline().decode()
            if line in ("\r\n", "\n", ""):
                break
            k, _, v = line.partition(":")
            headers[k.strip().lower()] = v.strip()
        n = int(headers.get("content-length", 0))
        return json.loads(p.stdout.read(n).decode())

    def wait_for(rid):
        while True:
            m = read_msg()
            if m.get("id") == rid:
                return m

    results = {}
    # initialize
    rid = req("initialize", {"processId": None, "rootUri": None,
                             "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}})
    m = wait_for(rid)
    caps = m["result"]["capabilities"]
    results["initialize"] = all(
        caps.get(k) is not None
        for k in ("completionProvider", "hoverProvider", "definitionProvider",
                  "typeDefinitionProvider", "referencesProvider", "renameProvider",
                  "documentFormattingProvider", "foldingRangeProvider",
                  "signatureHelpProvider", "documentSymbolProvider",
                  "workspaceSymbolProvider", "semanticTokensProvider"))
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    # didOpen
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
          "params": {"textDocument": {"uri": URI, "languageId": "hone",
                                      "version": 1, "text": SRC}}})
    # 等诊断推送
    diag = read_msg()
    results["diagnostics"] = diag.get("method") == "textDocument/publishDiagnostics"

    # documentSymbol
    rid = req("textDocument/documentSymbol", {"textDocument": {"uri": URI}})
    syms = wait_for(rid)["result"]
    names = [s["name"] for s in syms]
    results["documentSymbol"] = "add" in names and "Point" in names and "Box" in names

    # references: 在 add 调用处（行 12 0-based：y = add(x, 5);，add 在 col 4-6）
    rid = req("textDocument/references", {"textDocument": {"uri": URI},
                                          "position": {"line": 12, "character": 5},
                                          "context": {"includeDeclaration": True}})
    refs = wait_for(rid)["result"]
    ref_lines = sorted(r["range"]["start"]["line"] for r in refs)
    results["references"] = len(refs) >= 2 and all("uri" in r for r in refs) and 2 in ref_lines

    # rename: 把 add 改名为 plus，应覆盖定义(行2) + 调用(行12)
    rid = req("textDocument/rename", {"textDocument": {"uri": URI},
                                      "position": {"line": 12, "character": 5},
                                      "newName": "plus"})
    edit = wait_for(rid)["result"]
    edits = edit.get("changes", {}).get(URI, [])
    texts = [e["newText"] for e in edits]
    edit_lines = sorted(e["range"]["start"]["line"] for e in edits)
    results["rename"] = texts.count("plus") >= 2 and 2 in edit_lines

    # signatureHelp: 光标在 add(x, 5 的第二参数处（行 12 col 11，即 '5'）
    rid = req("textDocument/signatureHelp", {"textDocument": {"uri": URI},
                                             "position": {"line": 12, "character": 11}})
    sh = wait_for(rid)["result"]
    sig = (sh or {}).get("signatures", [{}])[0].get("label", "")
    results["signatureHelp"] = "add" in sig and "a: int" in sig and "b: int" in sig

    # foldingRange: fn 体 行 2-4（0-based）应可折叠
    rid = req("textDocument/foldingRange", {"textDocument": {"uri": URI}})
    fr = wait_for(rid)["result"]
    results["foldingRange"] = any(
        r.get("startLine") == 2 and r.get("endLine") == 4 for r in fr)

    # formatting
    rid = req("textDocument/formatting", {"textDocument": {"uri": URI},
                                          "options": {"tabSize": 4, "insertSpaces": True}})
    fmt = wait_for(rid)["result"]
    results["formatting"] = fmt is not None and isinstance(fmt, list)

    # workspace/symbol
    rid = req("workspace/symbol", {"query": "Poi"})
    wsyms = wait_for(rid)["result"]
    results["workspaceSymbol"] = any(s["name"] == "Point" for s in wsyms)

    # semanticTokens/full
    rid = req("textDocument/semanticTokens/full", {"textDocument": {"uri": URI}})
    st = wait_for(rid)["result"]
    data = st.get("data", []) if st else []
    results["semanticTokens"] = len(data) > 0

    # 未声明能力应返回 -32601（documentHighlight 未实现）
    rid = req("textDocument/documentHighlight", {"textDocument": {"uri": URI},
                                                 "position": {"line": 16, "character": 8}})
    err = wait_for(rid)
    results["methodNotFound"] = err.get("error", {}).get("code") == -32601

    send({"jsonrpc": "2.0", "method": "shutdown"})
    wait_for(None) if False else None
    # shutdown 的 id 是最后那个请求 id
    send({"jsonrpc": "2.0", "method": "exit"})
    p.wait(timeout=5)

    failed = [k for k, v in results.items() if not v]
    print(json.dumps(results, indent=2, ensure_ascii=False))
    if failed:
        print("FAILED:", failed)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
