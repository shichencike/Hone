# -*- coding: utf-8 -*-
"""Hone 官网重做：把现有 12 个页面原样迁移到新外壳（左侧目录 + 移动抽屉 + 冷蓝双主题）。
内容（<main> 内）逐字保留，仅换壳与排版；.toc 移入侧栏作「本页目录」。"""
import os, re

SITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "官网")
VERSION = "0.7.12"

# 分组侧栏导航： (组标题, [(文件名, 显示名), ...])
NAV = [
    ("入门", [("index.html", "首页"), ("install.html", "安装"), ("download.html", "下载")]),
    ("学习", [("tutorial.html", "教程"), ("examples.html", "示例")]),
    ("参考", [("docs.html", "文档"), ("language.html", "语言参考"), ("stdlib.html", "标准库"), ("perf.html", "性能")]),
    ("项目", [("changelog.html", "更新日志"), ("faq.html", "FAQ"), ("about.html", "关于")]),
]

PAGES = [f for _, items in NAV for f, _ in items]


def strip_node_ids(s):
    return re.sub(r'\s*data-page-node-id="[^"]*"', "", s)


def extract_main(html):
    m = re.search(r"<main[^>]*>(.*?)</main>", html, re.S)
    return m.group(1) if m else ""


def extract_toc(content):
    """返回 (toc_ol_html_or_None, content_without_toc)"""
    m = re.search(r'<div class="toc"[^>]*>(.*?)</div>', content, re.S)
    if not m:
        return None, content
    inner = m.group(1)
    ol = re.search(r"<ol[^>]*>.*?</ol>", inner, re.S)
    ol_html = ol.group(0) if ol else inner
    content2 = content[:m.start()] + content[m.end():]
    return ol_html, content2


def extract_page_scripts(html):
    """保留非主题初始化的内联脚本（如 searchDocs），丢弃旧主题脚本。"""
    blocks = re.findall(r"<script>(.*?)</script>", html, re.S)
    out = []
    for b in blocks:
        if "hone-theme" in b:
            continue  # 旧主题初始化，已由 site.js 接管
        if b.strip():
            out.append("<script>" + b + "</script>")
    return "\n".join(out)


def build_shell(title, description, content, page_toc, active, page_scripts):
    desc = description.replace('"', "&quot;")
    nav_html = []
    for group, items in NAV:
        links = []
        for fn, label in items:
            cls = ' class="active"' if fn == active else ""
            links.append('      <a href="%s"%s>%s</a>' % (fn, cls, label))
        nav_html.append(
            '    <div class="nav-group">\n'
            '      <div class="nav-title">%s</div>\n%s\n'
            "    </div>" % (group, "\n".join(links))
        )
    nav_block = "\n".join(nav_html)
    toc_block = ""
    if page_toc:
        toc_block = (
            '    <div class="page-toc">\n'
            '      <div class="nav-title">本页目录</div>\n'
            "      %s\n"
            "    </div>" % page_toc
        )
    return """<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title}</title>
<meta name="description" content="{desc}">
<meta name="theme-color" content="#0b0e14">
<link rel="icon" href="favicon.png" type="image/png">
<link rel="stylesheet" href="style.css">
</head>
<body>
<header>
  <button class="menu-btn" onclick="toggleNav()" aria-label="菜单">&#9776;</button>
  <span class="logo"><span class="mark">&#9889;</span> Hone</span>
  <span class="spacer"></span>
  <span class="right">
    <span class="ver">v{ver}</span>
    <button class="theme-toggle" onclick="toggleTheme()" title="切换主题" aria-label="切换主题"><span class="ti-light">&#9728;</span><span class="ti-dark">&#127769;</span></button>
  </span>
</header>

<aside class="sidebar" id="sidebar">
  <nav>
{nav}
{toc}
  </nav>
</aside>
<div class="sidebar-backdrop" id="navBackdrop"></div>

<main>
{content}
</main>

<footer>
  Hone v{ver} &middot; MIT License &middot; <span class="sword">&#128481;</span> Hone &ndash; 为效率而生，为乐趣而造
  &middot; <a href="https://github.com/shichencike/Hone">GitHub</a>
</footer>

<script src="site.js"></script>
{scripts}
</body>
</html>
""".format(title=title, desc=desc, ver=VERSION, nav=nav_block, toc=toc_block,
           content=content, scripts=page_scripts)


def main():
    for fn in PAGES:
        path = os.path.join(SITE, fn)
        with open(path, "r", encoding="utf-8") as f:
            html = f.read()
        title_m = re.search(r"<title>(.*?)</title>", html, re.S)
        title = title_m.group(1).strip() if title_m else "Hone"
        desc_m = re.search(r'<meta name="description" content="(.*?)"', html, re.S)
        description = desc_m.group(1).strip() if desc_m else "Hone 编程语言官网"
        content = extract_main(html)
        toc_ol, content = extract_toc(content)
        content = strip_node_ids(content)
        if toc_ol:
            toc_ol = strip_node_ids(toc_ol)
        scripts = extract_page_scripts(html)
        out = build_shell(title, description, content, toc_ol, fn, scripts)
        with open(path, "w", encoding="utf-8") as f:
            f.write(out)
        print("OK  %-14s  toc=%s  scripts=%d chars" % (fn, "Y" if toc_ol else "-", len(scripts)))


if __name__ == "__main__":
    main()
