#!/usr/bin/env bash
# 只需自建 app 的 image tar（db 直接由 chefer 從 registry 拉 redis:7.2-alpine，免 docker save）：
#   examples/demo/images/app.tar  （自建：python + redis client + server.py）
# 之後即可 `chefer build examples/demo/appcipe.yml`（打包時會自動拉 redis）。
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # examples/demo
images="$here/images"
platform="${CHEFER_DEMO_PLATFORM:-linux/amd64}"           # 或 linux/arm64
app_tag="chefer-demo-app:latest"

mkdir -p "$images"

echo "==> 建置 app 映像（platform=$platform）"
docker build --platform "$platform" -t "$app_tag" "$here/app"
docker save "$app_tag" -o "$images/app.tar"

echo "==> 完成：$images/app.tar（db 會在 chefer build 時從 registry 拉）"
echo "    接著：cargo run -p chefer-cli -- build examples/demo/appcipe.yml --out dist"
