# 產生 PySide+redis demo 的兩個 image tar（Windows 版本）。
#   examples\pyside-redis\images\db.tar   （官方 redis:alpine）
#   examples\pyside-redis\images\gui.tar  （python-slim + PySide6 + redis client）
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot          # examples\pyside-redis
$images = Join-Path $here "images"
$platform = if ($env:CHEFER_DEMO_PLATFORM) { $env:CHEFER_DEMO_PLATFORM } else { "linux/amd64" }
$guiTag = "chefer-pyside-gui:latest"
$dbTag = "redis:alpine"

New-Item -ItemType Directory -Force $images | Out-Null

Write-Host "==> 取得 redis 映像（$dbTag, platform=$platform）"
docker pull --platform $platform $dbTag
docker save $dbTag -o (Join-Path $images "db.tar")

Write-Host "==> 建置 GUI 映像（PySide6, platform=$platform）— 會下載 Qt，較大、需數分鐘"
docker build --platform $platform -t $guiTag (Join-Path $here "gui")
docker save $guiTag -o (Join-Path $images "gui.tar")

Write-Host "==> 完成：$images\db.tar、$images\gui.tar"
Write-Host "    接著：cargo run -p chefer-cli -- build examples\pyside-redis\appcipe.yml --out dist"
