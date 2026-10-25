#!/usr/bin/env python3
# mock_sse.py - 本地 SSE 测试服务器（模拟 OpenAI Chat Completions 流式响应）
# 用法: python mock_sse.py [port]   默认 8897
# 返回多个 SSE 事件 + [DONE]，并打印收到的请求体（供调试）
import http.server
import json
import sys
import threading
import time

# Windows 控制台默认 GBK，显式用 UTF-8 输出调试日志
if hasattr(sys.stderr, "reconfigure"):
    try:
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8897


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8", "replace")
        sys.stderr.write("REQ %s %s\n" % (self.path, body[:400]))
        try:
            req = json.loads(body) if body else {}
            model = req.get("model", "mock")
            messages = req.get("messages", [])
            user_text = ""
            for m in messages:
                if m.get("role") == "user":
                    user_text = m.get("content", "")
            stream = req.get("stream", False)
        except Exception as e:
            model = "mock"
            user_text = "parse-error: %s" % e
            stream = False

        reply = "收到！你说的内容是：%s" % user_text
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        # 流式必须 keep-alive；非流式尊重客户端 Connection: close（hone http.request 依赖 EOF 结束）
        if stream:
            self.send_header("Connection", "keep-alive")
        else:
            self.send_header("Connection", "close")
        if stream:
            self.end_headers()
            # 逐字推送（模拟真实流式）
            for ch in reply:
                evt = {"choices": [{"delta": {"content": ch}, "index": 0}]}
                self.wfile.write(("data: %s\n\n" % json.dumps(evt, ensure_ascii=False)).encode("utf-8"))
                self.wfile.flush()
                time.sleep(0.01)
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            payload = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": reply}, "index": 0}]},
                ensure_ascii=False,
            )
            self.send_header("Content-Length", str(len(payload.encode("utf-8"))))
            self.end_headers()
            self.wfile.write(payload.encode("utf-8"))

    def log_message(self, fmt, *args):
        pass


if __name__ == "__main__":
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print("mock SSE server on http://127.0.0.1:%d" % PORT, flush=True)
    srv.serve_forever()
