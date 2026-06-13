#!/usr/bin/env bash
# 產生 demo 所需的兩個 image tar（docker-archive）：
#   examples/demo/images/app.tar  （自建：python + redis client + server.py）
#   examples/demo/images/db.tar   （redis 官方映像）
# 之後即可 `chefer build examples/demo/appcipe.yml`。
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # examples/demo
images="$here/images"
platform="${CHEFER_DEMO_PLATFORM:-linux/amd64}"           # 或 linux/arm64
app_tag="chefer-demo-app:latest"
db_tag="chefer-demo-db:latest"

mkdir -p "$images"

echo "==> 建置 app 映像（platform=$platform）"
docker build --platform "$platform" -t "$app_tag" "$here/app"
docker save "$app_tag" -o "$images/app.tar"

echo "==> 建置 db 映像（alpine+redis，platform=$platform）"
docker build --platform "$platform" -t "$db_tag" "$here/db"
docker save "$db_tag" -o "$images/db.tar"

echo "==> 完成：$images/app.tar、$images/db.tar"
echo "    接著：cargo run -p chefer-cli -- build examples/demo/appcipe.yml --out dist"
