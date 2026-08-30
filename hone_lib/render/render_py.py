#!/usr/bin/env python3
# -*- coding: utf-8 -*-
# hone_lib/render/render_py.py - Hone 爬虫库渲染后端（python + playwright）
# 用法: python render_py.py <url>
# 输出: 单行 JSON {"status":200,"headers":{...},"url":"...","html":"..."}
# 安装: pip install playwright && playwright install chromium

import sys
import json

def main():
    if len(sys.argv) < 2:
        sys.stderr.write("usage: python render_py.py <url>\n")
        sys.exit(1)
    url = sys.argv[1]

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        sys.stderr.write("render_py.py: 未安装 playwright，请运行: pip install playwright && playwright install chromium\n")
        sys.exit(1)

    with sync_playwright() as p:
        browser = p.chromium.launch(args=["--no-sandbox", "--disable-dev-shm-usage"])
        try:
            page = browser.new_page(
                user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
            )
            resp = page.goto(url, wait_until="networkidle", timeout=30000)
            html = page.content()
            final_url = page.url
            status = resp.status if resp else 0
            headers = dict(resp.headers) if resp else {}
            # 单行 JSON 输出到 stdout
            sys.stdout.write(json.dumps({"status": status, "headers": headers, "url": final_url, "html": html}, ensure_ascii=False) + "\n")
        finally:
            browser.close()

if __name__ == "__main__":
    main()
