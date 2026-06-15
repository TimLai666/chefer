#!/usr/bin/env bash
# 只需自建 GUI 的 image tar（db 由 chefer 從 registry 拉 redis:7.2-alpine，免 docker save）：
#   examples/pyside-redis/images/gui.tar  （python-slim + PySide6 + redis client）
# 之後即可 `chefer build examples/pyside-redis/appcipe.yml`（打包時會自動拉 redis）。
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # examples/pyside-redis
images="$here/images"
platform="${CHEFER_DEMO_PLATFORM:-linux/amd64}"
gui_tag="chefer-pyside-gui:latest"

mkdir -p "$images"

echo "==> 建置 GUI 映像（PySide6, platform=$platform）— 會下載 Qt，較大、需數分鐘"
docker build --platform "$platform" -t "$gui_tag" "$here/gui"
docker save "$gui_tag" -o "$images/gui.tar"

echo "==> 完成：$images/gui.tar（db 會在 chefer build 時從 registry 拉）"
echo "    接著：cargo run -p chefer-cli -- build examples/pyside-redis/appcipe.yml --out dist"
