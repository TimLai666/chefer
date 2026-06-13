# 產生 demo 所需的兩個 image tar（docker-archive）。Windows 版本。
#   examples\demo\images\app.tar （自建：python + redis client + server.py）
#   examples\demo\images\db.tar  （redis 官方映像）
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot          # examples\demo
$images = Join-Path $here "images"
$platform = if ($env:CHEFER_DEMO_PLATFORM) { $env:CHEFER_DEMO_PLATFORM } else { "linux/amd64" }
$appTag = "chefer-demo-app:latest"
$dbTag = "chefer-demo-db:latest"

New-Item -ItemType Directory -Force $images | Out-Null

Write-Host "==> 建置 app 映像（platform=$platform）"
docker build --platform $platform -t $appTag (Join-Path $here "app")
docker save $appTag -o (Join-Path $images "app.tar")

Write-Host "==> 建置 db 映像（alpine+redis，platform=$platform）"
docker build --platform $platform -t $dbTag (Join-Path $here "db")
docker save $dbTag -o (Join-Path $images "db.tar")

Write-Host "==> 完成：$images\app.tar、$images\db.tar"
Write-Host "    接著：cargo run -p chefer-cli -- build examples\demo\appcipe.yml --out dist"
