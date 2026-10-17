# -*- coding: utf-8 -*-
"""从 CHANGELOG.md 重新生成 官网/changelog.html（新外壳，版本号由源文件决定）。"""
import os, re
from redesign_build import build_shell

ROOT = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(ROOT, "CHANGELOG.md")
DST = os.path.join(ROOT, "官网", "changelog.html")

FLAG_MAP = {"新增": "add", "变更": "chg", "修复": "fix", "文档": "doc", "移除": "rem"}


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def fmt(text):
    text = esc(text)
    text = re.sub(r"`([^`]+)`", r'<code class="inline">\1</code>', text)
    text = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", text)
    return text


def build_tree(bullets):
    root = []
    stack = [(-1, root)]
    for indent, text in bullets:
        node = {"text": text, "children": []}
        while stack and stack[-1][0] >= indent:
            stack.pop()
        stack[-1][1].append(node)
        stack.append((indent, node["children"]))
    return root


def render_tree(nodes):
    if not nodes:
        return ""
    out = ["<ul>"]
    for nd in nodes:
        if nd["children"]:
            out.append("<li>" + fmt(nd["text"]) + render_tree(nd["children"]) + "</li>")
        else:
            out.append("<li>" + fmt(nd["text"]) + "</li>")
    out.append("</ul>")
    return "\n".join(out)


def parse():
    lines = open(SRC, "r", encoding="utf-8").read().splitlines()
    releases = []
    cur = None
    sub = None
    bullet_idx = None  # sub["bullets"] 里「当前 bullet」的下标（续行并入它）
    for line in lines:
        if line.startswith("## "):
            if cur:
                releases.append(cur)
            # 版本与日期之间的分隔符可能是 ASCII 半角 `-`，也可能是破折号 `—` / `–`。
            # 只认 `-` 时，v0.8.0（用 em dash 写）的日期会被静默丢掉——官网上它是唯一
            # 没有 <span class="date"> 的版本块。
            m = re.match(r"##\s+\[(.*?)\](?:\s+[-\u2014\u2013]\s+(.*))?", line)
            ver = m.group(1) if m else line[3:].strip()
            date = (m.group(2) or "").strip()
            cur = {"ver": ver, "date": date, "subs": []}
            sub = None
            bullet_idx = None
        elif line.startswith("### "):
            if cur:
                name = line[4:].strip()
                sub = {"name": name, "flag": FLAG_MAP.get(name, "doc"), "bullets": []}
                cur["subs"].append(sub)
                bullet_idx = None
        elif re.match(r"^\s*-\s", line):
            if sub:
                indent = len(line) - len(line.lstrip())
                text = re.sub(r"^\s*-\s", "", line).rstrip()
                sub["bullets"].append([indent, text])
                bullet_idx = len(sub["bullets"]) - 1
        elif not line.strip():
            bullet_idx = None
        elif sub and bullet_idx is not None:
            # CHANGELOG 里长条目折行，续行（无前导 `-`）并入当前 bullet——
            # 丢掉续行会渲染成半句话（旧实现在此截断）。
            sub["bullets"][bullet_idx][1] += " " + line.strip()
    if cur:
        releases.append(cur)
    # [indent, text] → (indent, text)
    for r in releases:
        for s in r["subs"]:
            s["bullets"] = [(i, t) for i, t in s["bullets"]]
    return releases


def render_release(r):
    s = ['<div class="release">']
    badge = ' <span class="badge-new">开发中</span>' if r["ver"].lower() == "unreleased" else ""
    s.append('<span class="ver-tag">%s</span>%s' % (esc(r["ver"]), badge))
    if r["date"]:
        s.append('<span class="date">%s</span>' % esc(r["date"]))
    for sub in r["subs"]:
        s.append('<h4><span class="flag %s">%s</span></h4>' % (sub["flag"], esc(sub["name"])))
        s.append(render_tree(build_tree(sub["bullets"])))
    s.append("</div>")
    return "\n".join(s)


def main():
    releases = parse()
    # 「最新版本」取第一个正式版本（跳过 Unreleased），避免硬编码随版本漂移。
    newest = next((r["ver"] for r in releases if r["ver"].lower() != "unreleased"), "")
    if not newest and releases:
        newest = releases[0]["ver"]
    body = ['<h2><span class="hash">#</span>更新日志</h2>']
    body.append(
        '<p>各版本的新增 / 变更 / 修复 / 文档汇总。最新版本见顶部 '
        '<span class="ver-tag">%s</span> 区块。</p>' % esc(newest)
    )
    for r in releases:
        body.append(render_release(r))
    content = "\n".join(body)
    html = build_shell(
        title="Hone 更新日志",
        description="Hone 编程语言各版本更新日志：新增、变更、修复与文档。",
        content=content,
        page_toc=None,
        active="changelog.html",
        page_scripts="",
    )
    with open(DST, "w", encoding="utf-8") as f:
        f.write(html)
    print("OK changelog.html  releases=%d  newest=%s" % (len(releases), newest))


if __name__ == "__main__":
    main()
