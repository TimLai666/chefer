#!/usr/bin/env bash
# 產生 GUI demo 所需的 image tar（docker-archive）：
#   examples/gui-demo/images/gui.tar  （debian-slim + x11-apps，CMD xeyes）
# 之後即可 `chefer build examples/gui-demo/appcipe.yml`。
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # examples/gui-demo
images="$here/images"
platform="${CHEFER_DEMO_PLATFORM:-linux/amd64}"           # 或 linux/arm64
tag="chefer-gui-demo:latest"

mkdir -p "$images"

echo "==> 建置 GUI 映像（xeyes，platform=$platform）"
docker build --platform "$platform" -t "$tag" "$here"
docker save "$tag" -o "$images/gui.tar"

echo "==> 完成：$images/gui.tar"
echo "    接著：cargo run -p chefer-cli -- build examples/gui-demo/appcipe.yml --out dist"
