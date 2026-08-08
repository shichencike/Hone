# Hone 安装脚本（Windows）
# 用法: irm https://github.com/shichencike/Hone/releases/latest/download/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "shichencike/Hone"   # TODO: 按实际 GitHub 仓库名修改
$Version = if ($env:HONE_VERSION) { $env:HONE_VERSION } else { "latest" }
$Asset = "hone-windows-x86_64.exe"

if ($Version -eq "latest") {
  $Base = "https://github.com/$Repo/releases/latest/download"
} else {
  $Base = "https://github.com/$Repo/releases/download/$Version"
}

$Dir = Join-Path $env:LOCALAPPDATA "hone\bin"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Out = Join-Path $Dir "hone.exe"

Write-Host "==> 下载 $Asset (v$Version)"
Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile "$Out.tmp"

$Checksums = (Invoke-WebRequest -UseBasicParsing "$Base/checksums.txt").Content
$Expected = ($Checksums -split "`r?`n" | Where-Object { $_ -match "\s$([regex]::Escape($Asset))\s*$" } | ForEach-Object { ($_ -split "\s+")[0] } | Select-Object -First 1)
$Actual = (Get-FileHash -Algorithm SHA256 "$Out.tmp").Hash.ToLower()
if (-not $Expected) { throw "checksums.txt 中找不到 $Asset 的条目" }
if ($Expected -ne $Actual) { throw "sha256 校验失败：期望 $Expected，实际 $Actual" }

Move-Item -Force "$Out.tmp" $Out

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$Dir*") {
  [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
  Write-Host "已将 $Dir 加入用户 PATH（新开的终端生效）"
}

Write-Host "==> 安装完成: $Out"
Write-Host "    重开终端后运行 hone --version"
