#!/bin/sh
# Hone 安装脚本（Linux / Termux）
# 用法: curl -fsSL https://github.com/shichencike/Hone/releases/latest/download/install.sh | sh
set -e

REPO="shichencike/Hone"          # TODO: 按实际 GitHub 仓库名修改
VERSION="${HONE_VERSION:-latest}"
# 镜像源（清华 TUNA github-release 镜像）。HONE_MIRROR 可覆盖为其他镜像；
# 下载时镜像优先，失败自动回退官方 GitHub。
MIRROR="${HONE_MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/github-release/$REPO}"
OFFICIAL="https://github.com/$REPO"

case "$(uname -s)" in
  Linux) OS="linux" ;;
  *)
    echo "install.sh 仅支持 Linux/Termux；Windows 请用 install.ps1，macOS 暂未发布" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="aarch64" ;;
  *)
    echo "不支持的架构: $(uname -m)" >&2
    exit 1
    ;;
esac

case "$OS-$ARCH" in
  linux-x86_64)  ASSET="hone-linux-x86_64" ;;
  linux-aarch64) ASSET="hone-termux-aarch64" ;;   # Termux / Android
  *)
    echo "暂不支持的平台: $OS-$ARCH" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = "latest" ]; then
  OFFICIAL_BASE="$OFFICIAL/releases/latest/download"
  MIRROR_BASE="$MIRROR/releases/latest/download"
else
  OFFICIAL_BASE="$OFFICIAL/releases/download/$VERSION"
  MIRROR_BASE="$MIRROR/releases/download/$VERSION"
fi

PREFIX="${HONE_PREFIX:-$HOME/.hone/bin}"
mkdir -p "$PREFIX"

# 下载文件：镜像优先，失败自动回退官方源。HONE_MIRROR=off 可强制只用官方源。
fetch() {
  file="$1"; out="$2"
  if [ "$MIRROR" = "off" ]; then
    curl -fsSL "$OFFICIAL_BASE/$file" -o "$out"
    return $?
  fi
  if curl -fsSL "$MIRROR_BASE/$file" -o "$out" 2>/dev/null; then
    echo "  [镜像源] $MIRROR_BASE/$file"
    return 0
  fi
  echo "  [镜像源失败，回退官方] $OFFICIAL_BASE/$file" >&2
  curl -fsSL "$OFFICIAL_BASE/$file" -o "$out"
  return $?
}

echo "==> 下载 $ASSET (v${VERSION})"
fetch "$ASSET" "$PREFIX/hone.tmp" || exit 1
fetch "checksums.txt" "$PREFIX/checksums.tmp" || exit 1

EXPECTED=$(awk -v a="$ASSET" '$2 == a { print $1 }' "$PREFIX/checksums.tmp")
ACTUAL=$(sha256sum "$PREFIX/hone.tmp" | awk '{ print $1 }')
if [ -z "$EXPECTED" ]; then
  echo "checksums.txt 中找不到 $ASSET 的条目" >&2
  exit 1
fi
if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "sha256 校验失败：期望 $EXPECTED，实际 $ACTUAL" >&2
  exit 1
fi
rm -f "$PREFIX/checksums.tmp"

chmod +x "$PREFIX/hone.tmp"
mv "$PREFIX/hone.tmp" "$PREFIX/hone"
echo "==> 安装完成: $PREFIX/hone"
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo "提示：请将 $PREFIX 加入 PATH，例如 export PATH=\"$PREFIX:\$PATH\"" ;;
esac
