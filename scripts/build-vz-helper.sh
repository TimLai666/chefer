#!/usr/bin/env bash
# 編譯 macOS vz 開機 helper（Swift）→ chefer-vz-helper-<arch>，並以 virtualization
# entitlement ad-hoc 簽章。僅能在 macOS 上執行（需 swiftc + Virtualization.framework + codesign）。
#
# 用法：scripts/build-vz-helper.sh [out_dir]   （預設 out_dir=dist）
# 產出：<out_dir>/chefer-vz-helper-<aarch64|x86_64>
set -Eeuo pipefail

die() { echo "error: $*" >&2; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || die "build-vz-helper.sh 只能在 macOS 上跑（需要 Virtualization.framework）"
command -v swiftc >/dev/null 2>&1 || die "找不到 swiftc；請安裝 Xcode 或 Command Line Tools"
command -v codesign >/dev/null 2>&1 || die "找不到 codesign"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-dist}"
mkdir -p "$out"

host_arch="$(uname -m)"
case "$host_arch" in
  arm64|aarch64) out_arch="aarch64"; swift_target="arm64-apple-macos13.0" ;;
  x86_64)        out_arch="x86_64";  swift_target="x86_64-apple-macos13.0" ;;
  *) die "不支援的 host 架構：$host_arch" ;;
esac

bin="$out/chefer-vz-helper-${out_arch}"
echo "==> swiftc → $bin (target=$swift_target)"
swiftc -O -target "$swift_target" -framework Virtualization \
  -o "$bin" "$root/vz-helper/main.swift"

echo "==> codesign（ad-hoc）+ virtualization entitlement"
codesign --force --options runtime \
  --entitlements "$root/vz-helper/vz-helper.entitlements" \
  --sign - "$bin"

echo "==> 完成：$bin"
codesign -d --entitlements - "$bin" 2>/dev/null || true
