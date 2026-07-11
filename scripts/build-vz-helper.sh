#!/usr/bin/env bash
# 編譯 macOS vz 開機 helper（Swift）→ chefer-vz-helper-<arch>，並以 virtualization
# entitlement ad-hoc 簽章。僅能在 macOS 上執行（需 swiftc + Virtualization.framework + codesign）。
#
# 用法：scripts/build-vz-helper.sh [--arch <aarch64|x86_64>] [out_dir]
#   --arch 省略時取 host 架構（uname -m）。swiftc 可在 Apple Silicon 上交叉編到 x86_64，
#   故 release 可在單一 arm64 runner 上分別產兩種架構的 helper。
# 產出：<out_dir>/chefer-vz-helper-<aarch64|x86_64>（已以 virtualization entitlement ad-hoc 簽章）
#
# 簽章身分：預設 ad-hoc（`-`），每次 build 身分都不同 → macOS TCC（如「區域網路」隱私權，
# 剪貼簿同步連 guest 會踩到）記不住授權、每個新 binary 都可能重新被閘。有 Apple Development
# / Developer ID 憑證時可設 CHEFER_CODESIGN_IDENTITY=<identity> 取得穩定簽章身分。
set -Eeuo pipefail

die() { echo "error: $*" >&2; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || die "build-vz-helper.sh 只能在 macOS 上跑（需要 Virtualization.framework）"
command -v swiftc >/dev/null 2>&1 || die "找不到 swiftc；請安裝 Xcode 或 Command Line Tools"
command -v codesign >/dev/null 2>&1 || die "找不到 codesign"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

arch=""
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) arch="${2:-}"; shift 2 ;;
    *) out="$1"; shift ;;
  esac
done
out="${out:-dist}"
[[ -n "$arch" ]] || arch="$(uname -m)"
mkdir -p "$out"

case "$arch" in
  arm64|aarch64) out_arch="aarch64"; swift_target="arm64-apple-macos13.0" ;;
  x86_64|amd64)  out_arch="x86_64";  swift_target="x86_64-apple-macos13.0" ;;
  *) die "不支援的架構：$arch" ;;
esac

bin="$out/chefer-vz-helper-${out_arch}"
echo "==> swiftc → $bin (target=$swift_target)"
swiftc -O -target "$swift_target" -framework Virtualization \
  -o "$bin" "$root/vz-helper/main.swift"

identity="${CHEFER_CODESIGN_IDENTITY:--}"
echo "==> codesign（identity: $identity）+ virtualization entitlement"
codesign --force --options runtime \
  --entitlements "$root/vz-helper/vz-helper.entitlements" \
  --sign "$identity" "$bin"

echo "==> 完成：$bin"
codesign -d --entitlements - "$bin" 2>/dev/null || true
