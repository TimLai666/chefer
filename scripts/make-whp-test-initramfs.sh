#!/bin/bash
# 從 test-init 的 musl release binary 建立測試用 initramfs（含 /dev/console）。
# 用法：在 WSL 或 Linux 內執行  bash scripts/make-whp-test-initramfs.sh
# 前置：cd crates/whp-helper/test-init && \
#        cargo build --release --target x86_64-unknown-linux-musl \
#          -Zbuild-std=core,alloc,std -Zbuild-std-features=panic_immediate_abort
set -ex

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INIT_BIN="$REPO_ROOT/crates/whp-helper/test-init/target/x86_64-unknown-linux-musl/release/init"

if [ ! -f "$INIT_BIN" ]; then
  echo "ERROR: init binary not found at $INIT_BIN"
  echo "Build it first:  cd crates/whp-helper/test-init && cargo build --release --target x86_64-unknown-linux-musl"
  exit 1
fi

TMPD="$(mktemp -d)"
trap 'rm -rf "$TMPD"' EXIT

cp "$INIT_BIN" "$TMPD/init"
chmod 755 "$TMPD/init"

# 建 /dev/console（kernel 需要它來開 init 的 stdio fd 0/1/2）
mkdir -p "$TMPD/dev" "$TMPD/proc" "$TMPD/sys"
mknod "$TMPD/dev/console" c 5 1 2>/dev/null || true

cd "$TMPD"
find . | cpio -o -H newc | gzip > "$REPO_ROOT/test-initramfs-real.bin"

echo "Created $REPO_ROOT/test-initramfs-real.bin"
ls -la "$REPO_ROOT/test-initramfs-real.bin"
