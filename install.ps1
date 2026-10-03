# ============================================================================
# Hone 一键安装脚本（Windows，PowerShell 5.1+）
# 用法: irm https://github.com/shichencike/Hone/releases/latest/download/install.ps1 | iex
#
# 功能特性：
#   - 优先使用系统自带 curl.exe（Windows 10 1803+ 已内置）下载：更快、支持断点续传与重试；
#     无 curl 时回退 Invoke-WebRequest
#   - 强制 TLS 1.2（旧系统默认 TLS 1.0 会被 GitHub 拒绝）
#   - 镜像优先 + 失败自动回退官方源（ghproxy.net 前缀代理，HONE_MIRROR 可覆盖）
#   - 幂等：已安装版本哈希一致时跳过下载，直接完成
#   - 原子安装：临时文件 + Move-Item 覆盖，中断不留半成品
#   - 幂等写入用户 PATH（精确匹配条目，不重复添加）
#   - 支持卸载：HONE_UNINSTALL=1
#
# 环境变量（均可选）：
#   HONE_VERSION    指定版本号（默认 latest）
#   HONE_MIRROR     镜像前缀（默认 ghproxy.net，前缀代理直接拼官方完整路径）；"off" = 只用官方源
#   HONE_PREFIX     安装目录（默认 %LOCALAPPDATA%\hone\bin）
#   HONE_UNINSTALL  1 = 卸载
#   HONE_NO_PATH    1 = 不修改用户 PATH
# ============================================================================
$ErrorActionPreference = "Stop"

# 强制 TLS 1.2（PowerShell 5.1 默认可能协商到 TLS 1.0，被 GitHub 拒绝）
try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

$Repo = "shichencike/Hone"   # TODO: 按实际 GitHub 仓库名修改
$Version = if ($env:HONE_VERSION) { $env:HONE_VERSION } else { "latest" }
$Asset = "hone-windows-x86_64.exe"
# ghproxy.net 为前缀代理：镜像地址 = 前缀 + 官方完整路径（无需 TUNA 的 /LatestRelease 目录结构）
$Mirror = if ($env:HONE_MIRROR) { $env:HONE_MIRROR } else { "https://ghproxy.net/" }
$Official = "https://github.com/$Repo"

if ($Version -eq "latest") {
  $OfficialBase = "$Official/releases/latest/download"
} else {
  $OfficialBase = "$Official/releases/download/$Version"
}
# 镜像 = 前缀代理 + 官方完整路径；HONE_MIRROR 可覆盖为任意前缀（如其他 ghproxy 站点）
$MirrorBase = "$($Mirror.TrimEnd('/'))/$OfficialBase"

$Dir = if ($env:HONE_PREFIX) { $env:HONE_PREFIX } else { Join-Path $env:LOCALAPPDATA "hone\bin" }
$Out = Join-Path $Dir "hone.exe"
$Tmp = Join-Path $Dir "hone.exe.tmp"
$ChecksTmp = Join-Path $Dir "checksums.txt.tmp"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null

# 检测系统 curl（Windows 10 1803+ 自带；比 Invoke-WebRequest 快且支持续传/重试）
$Curl = Get-Command curl.exe -ErrorAction SilentlyContinue

# 单次下载（curl 优先，回退 IWR；3 次重试）
function Invoke-Download {
  param([string]$Url, [string]$Dest)
  if ($Curl) {
    & $Curl.Source -fL --retry 3 --retry-delay 2 --connect-timeout 15 -C - --progress-bar $Url -o $Dest 2>$null
    if ($LASTEXITCODE -ne 0) { throw "curl 下载失败: $Url (退出码 $LASTEXITCODE)" }
  } else {
    $attempt = 0
    while ($true) {
      try {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Dest -TimeoutSec 120
        break
      } catch {
        $attempt++
        if ($attempt -ge 3) { throw "下载失败: $Url - $($_.Exception.Message)" }
        Start-Sleep -Seconds 2
      }
    }
  }
}

# 镜像优先，失败自动回退官方；HONE_MIRROR="off" 强制只用官方源
function Get-ReleaseFile {
  param([string]$File, [string]$Dest)
  if ($env:HONE_MIRROR -eq "off") {
    Invoke-Download "$OfficialBase/$File" $Dest
    return
  }
  try {
    Invoke-Download "$MirrorBase/$File" $Dest
    Write-Host "  [镜像源] $MirrorBase/$File"
  } catch {
    Write-Host "  [镜像源失败，回退官方] $OfficialBase/$File"
    Remove-Item -Force $Dest -ErrorAction SilentlyContinue
    Invoke-Download "$OfficialBase/$File" $Dest
  }
}

# ---------- 卸载模式 ----------
if ($env:HONE_UNINSTALL -eq "1") {
  Remove-Item -Force $Out -ErrorAction SilentlyContinue
  $up = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($up) {
    $dirNorm = $Dir.TrimEnd('\')
    $entries = $up -split ";" | Where-Object { $_ -and $_.TrimEnd('\') -ne $dirNorm }
    $new = $entries -join ";"
    if ($new -ne $up) {
      [Environment]::SetEnvironmentVariable("Path", $new, "User")
      Write-Host "已将 $Dir 从用户 PATH 移除"
    }
  }
  Remove-Item -Force $Tmp, $ChecksTmp -ErrorAction SilentlyContinue
  Write-Host "==> 已卸载（$Out 及 shell PATH 配置）"
  exit
}

Write-Host "==> 下载 $Asset (v$Version)"
Remove-Item -Force $Tmp, $ChecksTmp -ErrorAction SilentlyContinue

# 1) 先取 checksums.txt（小文件），确认发布存在并拿到目标哈希
Get-ReleaseFile "checksums.txt" $ChecksTmp
$Expected = (Get-Content $ChecksTmp | Where-Object { $_ -match "\s$([regex]::Escape($Asset))\s*$" } |
  ForEach-Object { ($_ -split "\s+")[0] } | Select-Object -First 1)
if (-not $Expected) { throw "checksums.txt 中找不到 $Asset 的条目" }

# 2) 已安装同版本（哈希一致）→ 跳过下载
if (Test-Path $Out) {
  $actual = (Get-FileHash -Algorithm SHA256 $Out).Hash.ToLower()
  if ($actual -eq $Expected) {
    Write-Host "==> 已安装最新版本（$Out），跳过下载"
    Remove-Item -Force $ChecksTmp -ErrorAction SilentlyContinue
    exit
  }
}

# 3) 下载 + sha256 校验
Get-ReleaseFile $Asset $Tmp
$actual = (Get-FileHash -Algorithm SHA256 $Tmp).Hash.ToLower()
if ($actual -ne $Expected) { throw "sha256 校验失败：期望 $Expected，实际 $actual" }

# 4) 原子安装（先临时名再 Move 覆盖）
Move-Item -Force $Tmp $Out
Remove-Item -Force $ChecksTmp -ErrorAction SilentlyContinue
$ver = & $Out --version 2>$null | Select-Object -First 1
Write-Host "==> 安装完成: $Out ($ver)"

# 5) 幂等写入用户 PATH（精确匹配条目，已存在则不重复添加）
if ($env:HONE_NO_PATH -ne "1") {
  $up = [Environment]::GetEnvironmentVariable("Path", "User")
  $dirNorm = $Dir.TrimEnd('\')
  $already = $false
  if ($up) {
    foreach ($entry in ($up -split ";")) {
      if ($entry -and $entry.TrimEnd('\') -ieq $dirNorm) { $already = $true; break }
    }
  }
  if (-not $already) {
    $new = if ($up) { "$up;$Dir" } else { $Dir }
    [Environment]::SetEnvironmentVariable("Path", $new, "User")
    Write-Host "已将 $Dir 加入用户 PATH（新开的终端生效）"
  }
}

Write-Host "==> 完成！重开终端后运行 hone --version"
