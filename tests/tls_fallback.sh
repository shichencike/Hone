#!/usr/bin/env bash
# tls_fallback.sh - TLS 根证书回退机制端到端测试
# 验证内容：
#   1. 内置根证书（webpki-roots）校验失败时，自动回退系统根证书 + 用户 CA（HONE_CA_BUNDLE / ~/.hn/ca.pem）
#   2. 用户 CA 文件可同时信任根证书与中间证书
#   3. 用户 CA 文件中的中间证书会注入链构建：服务器只发叶子证书（不随链发送中间证书）也能验证通过
# 依赖：hone 二进制（默认 target/debug/hone）、openssl、python3
# 用法：bash tests/tls_fallback.sh [/path/to/hone]

set -u
HONE="${1:-$(dirname "$0")/../target/debug/hone}"
HONE="$(cd "$(dirname "$HONE")" && pwd)/$(basename "$HONE")"
[ -x "$HONE" ] || { echo "hone binary not found: $HONE"; exit 1; }
command -v openssl >/dev/null || { echo "openssl required"; exit 1; }
command -v python >/dev/null || command -v python3 >/dev/null || { echo "python required"; exit 1; }
# 优先 python（部分机器 python3 是损坏的安装，实际可用的是 python）
PY=$(command -v python || command -v python3)

WORK=$(mktemp -d)
trap 'kill $SRV 2>/dev/null; rm -rf "$WORK"' EXIT
cd "$WORK"

# ---- 1. 生成 CA 链：根 CA → 中间 CA → 叶子证书（SAN=localhost） ----
# 用配置文件避免 MSYS2/Git Bash 把 -subj 的 /CN= 当路径转换
cat > root.cnf <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = Hone Test Root CA
EOF
cat > inter.cnf <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = Hone Test Intermediate CA
EOF
cat > leaf.cnf <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = localhost
EOF
printf 'basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\n' > inter.ext
printf 'basicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n' > leaf.ext

openssl req -x509 -newkey rsa:2048 -keyout root.key -out root.pem -days 365 -nodes -config root.cnf 2>/dev/null
openssl req -newkey rsa:2048 -keyout inter.key -out inter.csr -nodes -config inter.cnf 2>/dev/null
openssl x509 -req -in inter.csr -CA root.pem -CAkey root.key -CAcreateserial -out inter.pem -days 365 -extfile inter.ext 2>/dev/null
openssl req -newkey rsa:2048 -keyout leaf.key -out leaf.csr -nodes -config leaf.cnf 2>/dev/null
openssl x509 -req -in leaf.csr -CA inter.pem -CAkey inter.key -CAcreateserial -out leaf.pem -days 365 -extfile leaf.ext 2>/dev/null
cat leaf.pem inter.pem > chain.pem   # 完整链（服务器发送 叶子+中间证书）
touch empty.pem                      # 空信任文件（无信任场景）

# ---- 2. 最小 TLS 服务器（正确发送 close_notify） ----
cat > server.py <<'PY'
import socket, ssl, sys
port = int(sys.argv[1]); certfile = sys.argv[2]; keyfile = sys.argv[3]
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(certfile, keyfile)
body = b"<h1>hone tls test</h1>"
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(('127.0.0.1', port)); srv.listen(8)
while True:
    conn, _ = srv.accept()
    try:
        tls = ctx.wrap_socket(conn, server_side=True)
        tls.recv(65536)
        tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: " +
                    str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)
        tls.settimeout(1)
        try:
            tls.unwrap()   # 发送 TLS close_notify（正确关闭）
        except Exception:
            pass
        tls.close()
    except Exception:
        pass
PY

cat > t.hn <<'HN'
try {
    r = http_get("https://localhost:8443/");
    print("OK len=" + to_str(len(r)));
} catch e {
    print("ERR " + e.code);
}
HN

run() { # run <说明> <CA文件> <期望:OK|ERR>
    local desc="$1" cafile="$2" expect="$3" out
    out=$(HONE_CA_BUNDLE="$WORK/$cafile" "$HONE" "$WORK/t.hn" 2>/dev/null)
    if { [ "$expect" = OK ] && [[ "$out" == OK* ]]; } || { [ "$expect" = ERR ] && [[ "$out" == ERR* ]]; }; then
        echo "PASS  $desc -> $out"
    else
        echo "FAIL  $desc -> $out (expect $expect)"
        exit 1
    fi
}

# ---- 3. 测试（完整链：服务器发送 叶子+中间证书） ----
"$PY" server.py 8443 chain.pem leaf.key & SRV=$!
sleep 2
run "A 无信任拒绝"            empty.pem ERR
run "B 信任根证书"            root.pem  OK
run "C 信任中间证书"          inter.pem OK

# ---- 4. 测试（服务器只发叶子证书 → 验证中间证书注入） ----
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null; sleep 1
"$PY" server.py 8444 leaf.pem leaf.key & SRV=$!
sleep 2
sed -i 's/8443/8444/' t.hn
run "D 信任中间证书+只发叶子(注入)" inter.pem OK
run "E 只发叶子+无信任拒绝"         empty.pem ERR

echo "ALL TLS FALLBACK TESTS PASSED"
