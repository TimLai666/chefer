#!/usr/bin/env bash
# 產生 PySide+redis demo 的兩個 image tar：
#   examples/pyside-redis/images/db.tar   （官方 redis:alpine）
#   examples/pyside-redis/images/gui.tar  （python-slim + PySide6 + redis client）
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # examples/pyside-redis
images="$here/images"
platform="${CHEFER_DEMO_PLATFORM:-linux/amd64}"
gui_tag="chefer-pyside-gui:latest"
db_tag="redis:alpine"

mkdir -p "$images"

echo "==> 取得 redis 映像（$db_tag, platform=$platform）"
docker pull --platform "$platform" "$db_tag"
docker save "$db_tag" -o "$images/db.tar"

echo "==> 建置 GUI 映像（PySide6, platform=$platform）— 會下載 Qt，較大、需數分鐘"
docker build --platform "$platform" -t "$gui_tag" "$here/gui"
docker save "$gui_tag" -o "$images/gui.tar"

echo "==> 完成：$images/db.tar、$images/gui.tar"
echo "    接著：cargo run -p chefer-cli -- build examples/pyside-redis/appcipe.yml --out dist"
