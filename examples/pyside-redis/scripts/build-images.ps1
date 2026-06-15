# 只需自建 GUI 的 image tar（db 由 chefer 從 registry 拉 redis:7.2-alpine，免 docker save）。Windows 版本。
#   examples\pyside-redis\images\gui.tar  （python-slim + PySide6 + redis client）
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot          # examples\pyside-redis
$images = Join-Path $here "images"
$platform = if ($env:CHEFER_DEMO_PLATFORM) { $env:CHEFER_DEMO_PLATFORM } else { "linux/amd64" }
$guiTag = "chefer-pyside-gui:latest"

New-Item -ItemType Directory -Force $images | Out-Null

Write-Host "==> 建置 GUI 映像（PySide6, platform=$platform）— 會下載 Qt，較大、需數分鐘"
docker build --platform $platform -t $guiTag (Join-Path $here "gui")
docker save $guiTag -o (Join-Path $images "gui.tar")

Write-Host "==> 完成：$images\gui.tar（db 會在 chefer build 時從 registry 拉）"
Write-Host "    接著：cargo run -p chefer-cli -- build examples\pyside-redis\appcipe.yml --out dist"
