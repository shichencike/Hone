# Hone 安装脚本（Windows）
# 用法: irm https://github.com/shichencike/Hone/releases/latest/download/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "shichencike/Hone"   # TODO: 按实际 GitHub 仓库名修改
$Version = if ($env:HONE_VERSION) { $env:HONE_VERSION } else { "latest" }
$Asset = "hone-windows-x86_64.exe"

# 镜像源（清华 TUNA github-release 镜像）。$env:HONE_MIRROR 可覆盖为其他镜像；
# 下载时镜像优先，失败自动回退官方 GitHub。HONE_MIRROR="off" 强制只用官方源。
$Mirror = if ($env:HONE_MIRROR) { $env:HONE_MIRROR } else { "https://mirrors.tuna.tsinghua.edu.cn/github-release/$Repo" }
$Official = "https://github.com/$Repo"

if ($Version -eq "latest") {
  $OfficialBase = "$Official/releases/latest/download"
  $MirrorBase = "$Mirror/releases/latest/download"
} else {
  $OfficialBase = "$Official/releases/download/$Version"
  $MirrorBase = "$Mirror/releases/download/$Version"
}

# 下载文件：镜像优先，失败自动回退官方源
function Fetch-File {
  param([string]$File, [string]$Out)
  if ($env:HONE_MIRROR -eq "off") {
    Invoke-WebRequest -UseBasicParsing "$OfficialBase/$File" -OutFile $Out
    return
  }
  try {
    Invoke-WebRequest -UseBasicParsing "$MirrorBase/$File" -OutFile $Out
    Write-Host "  [镜像源] $MirrorBase/$File"
    return
  } catch {
    Write-Host "  [镜像源失败，回退官方] $OfficialBase/$File"
    Invoke-WebRequest -UseBasicParsing "$OfficialBase/$File" -OutFile $Out
  }
}

$Dir = Join-Path $env:LOCALAPPDATA "hone\bin"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Out = Join-Path $Dir "hone.exe"

Write-Host "==> 下载 $Asset (v$Version)"
Fetch-File $Asset "$Out.tmp"

$Checksums = Invoke-WebRequest -UseBasicParsing "$MirrorBase/checksums.txt" -OutFile (Join-Path $Dir "checksums.tmp") 2>$null
if ($LASTEXITCODE -ne 0 -or -not (Test-Path (Join-Path $Dir "checksums.tmp"))) {
  Invoke-WebRequest -UseBasicParsing "$OfficialBase/checksums.txt" -OutFile (Join-Path $Dir "checksums.tmp")
}
$Checksums = Get-Content (Join-Path $Dir "checksums.tmp") -Raw
$Expected = ($Checksums -split "`r?`n" | Where-Object { $_ -match "\s$([regex]::Escape($Asset))\s*$" } | ForEach-Object { ($_ -split "\s+")[0] } | Select-Object -First 1)
$Actual = (Get-FileHash -Algorithm SHA256 "$Out.tmp").Hash.ToLower()
if (-not $Expected) { throw "checksums.txt 中找不到 $Asset 的条目" }
if ($Expected -ne $Actual) { throw "sha256 校验失败：期望 $Expected，实际 $Actual" }

Remove-Item -Force (Join-Path $Dir "checksums.tmp") -ErrorAction SilentlyContinue
Move-Item -Force "$Out.tmp" $Out

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$Dir*") {
  [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
  Write-Host "已将 $Dir 加入用户 PATH（新开的终端生效）"
}

Write-Host "==> 安装完成: $Out"
Write-Host "    重开终端后运行 hone --version"
