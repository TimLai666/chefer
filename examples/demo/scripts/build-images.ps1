# 只需自建 app 的 image tar（db 由 chefer 從 registry 拉 redis:7.2-alpine，免 docker save）。Windows 版本。
#   examples\demo\images\app.tar （自建：python + redis client + server.py）
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot          # examples\demo
$images = Join-Path $here "images"
$platform = if ($env:CHEFER_DEMO_PLATFORM) { $env:CHEFER_DEMO_PLATFORM } else { "linux/amd64" }
$appTag = "chefer-demo-app:latest"

New-Item -ItemType Directory -Force $images | Out-Null

Write-Host "==> 建置 app 映像（platform=$platform）"
docker build --platform $platform -t $appTag (Join-Path $here "app")
docker save $appTag -o (Join-Path $images "app.tar")

Write-Host "==> 完成：$images\app.tar（db 會在 chefer build 時從 registry 拉）"
Write-Host "    接著：cargo run -p chefer-cli -- build examples\demo\appcipe.yml --out dist"
