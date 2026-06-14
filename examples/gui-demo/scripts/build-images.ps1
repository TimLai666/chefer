# 產生 GUI demo 所需的 image tar（docker-archive）。Windows 版本。
#   examples\gui-demo\images\gui.tar  （debian-slim + x11-apps，CMD xeyes）
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot          # examples\gui-demo
$images = Join-Path $here "images"
$platform = if ($env:CHEFER_DEMO_PLATFORM) { $env:CHEFER_DEMO_PLATFORM } else { "linux/amd64" }
$tag = "chefer-gui-demo:latest"

New-Item -ItemType Directory -Force $images | Out-Null

Write-Host "==> 建置 GUI 映像（xeyes，platform=$platform）"
docker build --platform $platform -t $tag $here
docker save $tag -o (Join-Path $images "gui.tar")

Write-Host "==> 完成：$images\gui.tar"
Write-Host "    接著：cargo run -p chefer-cli -- build examples\gui-demo\appcipe.yml --out dist"
