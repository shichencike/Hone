#!/bin/sh
# ============================================================================
# Hone 一键安装脚本（Linux / Termux / Android）
# 用法: curl -fsSL https://github.com/shichencike/Hone/releases/latest/download/install.sh | sh
#
# 功能特性：
#   - 镜像优先 + 失败自动回退官方源（ghproxy.net 前缀代理，HONE_MIRROR 可覆盖）
#   - 断点续传 + 自动重试（curl -C - --retry；无 curl 时回退 wget）
#   - 优先下载 .tar.gz 压缩包（体积更小），sha256 校验后解压安装
#   - 幂等：已安装版本哈希一致时跳过下载，直接完成
#   - 原子安装：临时文件 + mv 覆盖，中断不留半成品
#   - 自动配置 PATH（~/.profile ~/.bashrc ~/.zshrc，标记块幂等，HONE_NO_PATH 可关）
#   - 支持卸载：HONE_UNINSTALL=1
#
# 环境变量（均可选）：
#   HONE_VERSION    指定版本号（默认 latest）
#   HONE_MIRROR     镜像前缀（默认 ghproxy.net，前缀代理直接拼官方完整路径）；off = 只用官方源
#   HONE_PREFIX     安装目录（默认 ~/.hone/bin；Termux 下默认 $PREFIX/bin）
#   HONE_UNINSTALL  1 = 卸载
#   HONE_NO_PATH    1 = 不自动修改 shell 配置文件
# ============================================================================
set -e

REPO="shichencike/Hone"          # TODO: 按实际 GitHub 仓库名修改
VERSION="${HONE_VERSION:-latest}"
# ghproxy.net 为前缀代理：镜像地址 = 前缀 + 官方完整路径（无需 TUNA 的 /LatestRelease 目录结构）
MIRROR="${HONE_MIRROR:-https://ghproxy.net/}"
OFFICIAL="https://github.com/$REPO"

# ---------- 终端输出（非 TTY 自动去色） ----------
if [ -t 1 ]; then
  C_G='\033[32m'; C_Y='\033[33m'; C_R='\033[31m'; C_B='\033[1m'; C_X='\033[0m'
else
  C_G=''; C_Y=''; C_R=''; C_B=''; C_X=''
fi
ok()   { printf "%b[OK]%b %s\n" "$C_G" "$C_X" "$1"; }
warn() { printf "%b[!]%b %s\n" "$C_Y" "$C_X" "$1" >&2; }
die()  { printf "%b[ERROR]%b %s\n" "$C_R" "$C_X" "$1" >&2; exit 1; }

# ---------- 平台识别 ----------
case "$(uname -s)" in
  Linux) OS="linux" ;;
  *)
    die "install.sh 仅支持 Linux/Termux；Windows 请用 install.ps1，macOS 暂未发布"
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="aarch64" ;;
  *) die "不支持的架构: $(uname -m)" ;;
esac

case "$OS-$ARCH" in
  linux-x86_64)  ASSET="hone-linux-x86_64" ;;
  linux-aarch64) ASSET="hone-termux-aarch64" ;;   # Termux / Android
  *) die "暂不支持的平台: $OS-$ARCH" ;;
esac

# ---------- 下载工具 ----------
HAVE_CURL=0; HAVE_WGET=0
command -v curl >/dev/null 2>&1 && HAVE_CURL=1
command -v wget >/dev/null 2>&1 && HAVE_WGET=1
[ "$HAVE_CURL" = "1" ] || [ "$HAVE_WGET" = "1" ] || die "需要 curl 或 wget（apt install curl / pkg install curl）"

# sha256 工具（Termux/Alpine 等 busybox 环境也能用）
if command -v sha256sum >/dev/null 2>&1; then
  SHA="sha256sum"
elif command -v busybox >/dev/null 2>&1; then
  SHA="busybox sha256sum"
else
  die "缺少 sha256sum，请安装 coreutils（Termux: pkg install coreutils）"
fi

# 交互终端显示进度条，管道/CI 保持安静
if [ -t 2 ]; then DL_OPTS="--progress-bar"; else DL_OPTS="-sS"; fi

# ---------- 安装目录 ----------
# Termux 环境变量 $PREFIX=/data/data/com.termux/files/usr，装到 $PREFIX/bin 天然在 PATH
if [ -n "${HONE_PREFIX:-}" ]; then
  INSTALL_DIR="$HONE_PREFIX"
elif [ -n "${TERMUX_VERSION:-}" ] || [ "${PREFIX:-}" = "/data/data/com.termux/files/usr" ]; then
  INSTALL_DIR="${PREFIX:-/data/data/com.termux/files/usr}/bin"
else
  INSTALL_DIR="$HOME/.hone/bin"
fi

# ---------- 卸载模式 ----------
if [ "${HONE_UNINSTALL:-0}" = "1" ]; then
  rm -f "$INSTALL_DIR/hone"
  for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc" ] || continue
    tmp="${rc}.hone-clean"
    awk 'BEGIN{skip=0} /# >>> hone >>>/{skip=1} /# <<< hone <<</{skip=0; next} !skip{print}' "$rc" > "$tmp" && mv "$tmp" "$rc"
  done
  echo "==> 已卸载（$INSTALL_DIR/hone 及 shell PATH 配置）"
  exit 0
fi

# ---------- 下载地址 ----------
if [ "$VERSION" = "latest" ]; then
  OFFICIAL_BASE="$OFFICIAL/releases/latest/download"
else
  OFFICIAL_BASE="$OFFICIAL/releases/download/$VERSION"
fi
# 镜像 = 前缀代理 + 官方完整路径；HONE_MIRROR 可覆盖为任意前缀（如其他 ghproxy 站点）
MIRROR_BASE="${MIRROR%/}/$OFFICIAL_BASE"

# 单次下载（断点续传 + 重试）
dl() {
  url="$1"; out="$2"
  if [ "$HAVE_CURL" = "1" ]; then
    curl -fL --retry 3 --retry-delay 2 --connect-timeout 15 -C - $DL_OPTS "$url" -o "$out"
  else
    wget -c -q --tries=3 -T 30 -O "$out" "$url"
  fi
}

# 镜像优先，失败自动回退官方；HONE_MIRROR=off 强制只用官方源
fetch() {
  file="$1"; out="$2"
  if [ "$MIRROR" = "off" ]; then
    dl "$OFFICIAL_BASE/$file" "$out" || return 1
    return 0
  fi
  if dl "$MIRROR_BASE/$file" "$out" 2>/dev/null; then
    echo "  [镜像源] $MIRROR_BASE/$file"
    return 0
  fi
  echo "  [镜像源失败，回退官方] $OFFICIAL_BASE/$file" >&2
  rm -f "$out"
  dl "$OFFICIAL_BASE/$file" "$out"
}

# ---------- 主流程 ----------
mkdir -p "$INSTALL_DIR" 2>/dev/null || die "无法创建安装目录 $INSTALL_DIR（可用 HONE_PREFIX 指定其他目录）"
[ -w "$INSTALL_DIR" ] || die "安装目录不可写: $INSTALL_DIR（需 sudo，或 HONE_PREFIX 指定其他目录）"

TMPDL="${TMPDIR:-/tmp}/hone-dl.$$"
mkdir -p "$TMPDL"
trap 'rm -rf "$TMPDL"' EXIT INT TERM

echo "==> 下载 $ASSET (v$VERSION)"

# 1) 先取 checksums.txt（小文件），确认发布存在并拿到目标哈希
fetch "checksums.txt" "$TMPDL/checksums.txt" || die "下载 checksums.txt 失败"
EXPECTED_BIN=$(awk -v a="$ASSET" '$2 == a {print $1}' "$TMPDL/checksums.txt")
EXPECTED_TGZ=$(awk -v a="$ASSET.tar.gz" '$2 == a {print $1}' "$TMPDL/checksums.txt")
[ -n "$EXPECTED_BIN" ] || [ -n "$EXPECTED_TGZ" ] || die "checksums.txt 中找不到 $ASSET 的条目"

# 2) 已安装同版本（哈希一致）→ 跳过下载
if [ -x "$INSTALL_DIR/hone" ] && [ -n "$EXPECTED_BIN" ]; then
  ACTUAL=$($SHA "$INSTALL_DIR/hone" | awk '{print $1}')
  if [ "$ACTUAL" = "$EXPECTED_BIN" ]; then
    ok "已安装最新版本（$INSTALL_DIR/hone），跳过下载"
    exit 0
  fi
fi

# 3) 优先下载 .tar.gz 压缩包（更小）；没有则退回裸二进制
if [ -n "$EXPECTED_TGZ" ]; then
  fetch "$ASSET.tar.gz" "$TMPDL/hone.tgz" || die "下载 $ASSET.tar.gz 失败"
  ACTUAL=$($SHA "$TMPDL/hone.tgz" | awk '{print $1}')
  [ "$ACTUAL" = "$EXPECTED_TGZ" ] || die "sha256 校验失败（tar.gz）：期望 $EXPECTED_TGZ，实际 $ACTUAL"
  tar -xzf "$TMPDL/hone.tgz" -C "$TMPDL" || die "解压失败（tar 包损坏？）"
  mv "$TMPDL/$ASSET" "$TMPDL/hone"
else
  fetch "$ASSET" "$TMPDL/hone" || die "下载 $ASSET 失败"
  ACTUAL=$($SHA "$TMPDL/hone" | awk '{print $1}')
  [ "$ACTUAL" = "$EXPECTED_BIN" ] || die "sha256 校验失败：期望 $EXPECTED_BIN，实际 $ACTUAL"
fi

# 4) 原子安装（先落临时名再 mv 覆盖）
chmod +x "$TMPDL/hone"
mv -f "$TMPDL/hone" "$INSTALL_DIR/hone"
VER=$("$INSTALL_DIR/hone" --version 2>/dev/null | head -n1)
ok "安装完成: $INSTALL_DIR/hone ($VER)"

# 5) 自动配置 PATH（标记块幂等；已在 PATH 或 HONE_NO_PATH=1 时跳过）
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    if [ "${HONE_NO_PATH:-0}" != "1" ]; then
      add_block() {
        rc="$1"
        [ -f "$rc" ] || return 1
        grep -q '# >>> hone >>>' "$rc" 2>/dev/null && return 0
        printf '\n# >>> hone >>>\nexport PATH="%s:$PATH"\n# <<< hone <<<\n' "$INSTALL_DIR" >> "$rc"
        echo "    已写入 PATH 配置: $rc"
      }
      done_=0
      for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
        add_block "$rc" && done_=1
      done
      if [ "$done_" = "0" ]; then
        printf '\n# >>> hone >>>\nexport PATH="%s:$PATH"\n# <<< hone <<<\n' "$INSTALL_DIR" > "$HOME/.profile"
        echo "    已创建 $HOME/.profile 并写入 PATH"
      fi
      warn "请重新打开终端（或 source ~/.bashrc）后使用 hone"
    fi
    ;;
esac

echo "==> 完成！运行 hone --version 验证"
