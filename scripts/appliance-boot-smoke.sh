#!/usr/bin/env bash
set -Eeuo pipefail

die() {
  echo "::error::$*" >&2
  exit 1
}

note() {
  echo "==> $*" >&2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "缺少必要命令：$1"
}

usage() {
  printf '%s\n' \
'用法：scripts/appliance-boot-smoke.sh --arch <x86_64|aarch64> [--dir <appliance-dir>]' \
'' \
'以 QEMU 裸開機 Chefer Linux appliance（不掛 virtiofs、不給 bundle），驗證 boot 級契約：' \
'kernel 能啟動、initramfs 的 busybox 可執行（架構正確）、chefer-init 跑到 cmdline 檢查、' \
'console 印出 CHEFER_GUEST_EXIT 標記並正常關機。' \
'' \
'不帶 chefer.bundle_dir 開機時，init 的既定行為是 fail(125)（見 scripts/appliance/init），' \
'故本 smoke 斷言 console 出現 [chefer-init] 與 CHEFER_GUEST_EXIT=125。若 init 修改了' \
'該契約（cmdline 檢查順序 / exit code），這裡要跟著更新。' \
'' \
'動機：擋「initramfs 是錯誤架構」這類 boot 級 regression —— v0.4.0 的 aarch64 initramfs' \
'內嵌了 x86-64 busybox（x86_64 host 交叉建置時抄容器自身 /bin/busybox），guest exec /init' \
'即 ENOEXEC panic，一路活到 release 才在實體 Mac 上被抓到。aarch64 在 x86_64 host 以' \
'TCG 模擬開機即可驗證，無需 arm64 硬體。' \
'' \
'參數：' \
'  --arch <x86_64|aarch64>  appliance 架構（必填）' \
'  --dir <dir>              含 chefer-vmlinuz-<arch> / chefer-initramfs-<arch> 的目錄' \
'                           （預設：dist/appliance，即 build-appliance.sh 的預設輸出）' \
'' \
'可選環境變數：' \
'  CHEFER_SMOKE_TIMEOUT     等待 console 標記的秒數（預設：300；TCG 下裸開機通常 <60s）' >&2
}

repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd
}

normalize_arch() {
  case "$1" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) die "不支援的 appliance 架構：$1（預期 x86_64 或 aarch64）" ;;
  esac
}

# 讀 ELF header 的 e_machine（offset 18–19，little-endian）：62 = x86-64、183 = AArch64。
# 直接讀 bytes，不依賴 file(1) 的輸出字串格式。
elf_machine() {
  local b0 b1
  read -r b0 b1 < <(od -An -j18 -N2 -tu1 "$1")
  echo $((b0 + 256 * b1))
}

# 靜態預檢：從 initramfs 解出 bin/busybox，驗證其 ELF 架構與目標一致。
# 錯架構時在這裡給出可讀錯誤，而不是等 QEMU 裡一段 kernel panic dump。
# 解不出來（無 cpio / 格式變動）只跳過不失敗 —— 後面的開機驗證才是權威檢查。
check_initramfs_busybox_arch() {
  local initramfs="$1"
  local arch="$2"
  local tmp="$3"
  if ! command -v cpio >/dev/null 2>&1; then
    note "找不到 cpio，跳過 initramfs busybox 靜態架構檢查（開機驗證仍會擋住錯架構）"
    return 0
  fi
  if ! zcat "$initramfs" 2>/dev/null | cpio -i --quiet --to-stdout '*bin/busybox' >"$tmp/busybox" 2>/dev/null \
    || [[ ! -s "$tmp/busybox" ]]; then
    note "無法從 initramfs 解出 bin/busybox，跳過靜態架構檢查（開機驗證仍會擋住錯架構）"
    return 0
  fi
  local magic
  magic="$(od -An -j0 -N4 -tx1 "$tmp/busybox" | tr -d ' ')"
  [[ "$magic" == "7f454c46" ]] || die "initramfs 的 bin/busybox 不是 ELF（開頭 bytes：${magic}）"
  local expected got
  case "$arch" in
    x86_64) expected=62 ;;
    aarch64) expected=183 ;;
  esac
  got="$(elf_machine "$tmp/busybox")"
  if [[ "$got" != "$expected" ]]; then
    die "initramfs 的 busybox 架構錯誤：ELF e_machine=${got}，預期 ${expected}（${arch}）。交叉建置時 build-inside-container.sh 必須抓目標架構的 busybox-static，不能抄容器自身的 /bin/busybox（v0.4.0 regression）"
  fi
  note "initramfs busybox 架構正確（ELF e_machine=${got} = ${arch}）"
}

QEMU_PID=""
WORK_DIR=""

cleanup() {
  if [[ -n "${QEMU_PID:-}" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
    kill -TERM "$QEMU_PID" 2>/dev/null || true
    wait "$QEMU_PID" 2>/dev/null || true
  fi
  QEMU_PID=""
  if [[ -n "${WORK_DIR:-}" && -d "$WORK_DIR" && "$WORK_DIR" == *"/chefer-appliance-smoke."* ]]; then
    rm -rf -- "$WORK_DIR"
  fi
}

dump_logs() {
  local console_log="$1"
  local stderr_log="$2"
  echo "===== guest console（${console_log}）=====" >&2
  cat "$console_log" >&2 2>/dev/null || true
  echo "===== qemu stderr（${stderr_log}）=====" >&2
  cat "$stderr_log" >&2 2>/dev/null || true
}

main() {
  local arch=""
  local dir=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --arch)
        [[ $# -ge 2 ]] || die "--arch requires a value"
        arch="$(normalize_arch "$2")"
        shift 2
        ;;
      --dir)
        [[ $# -ge 2 ]] || die "--dir requires a value"
        dir="$2"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "未知參數：$1"
        ;;
    esac
  done
  [[ -n "$arch" ]] || die "請以 --arch 指定 appliance 架構（x86_64 或 aarch64）"
  if [[ -z "$dir" ]]; then
    dir="$(repo_root)/dist/appliance"
  fi

  local kernel="$dir/chefer-vmlinuz-$arch"
  local initramfs="$dir/chefer-initramfs-$arch"
  [[ -f "$kernel" ]] || die "找不到 appliance kernel：$kernel"
  [[ -f "$initramfs" ]] || die "找不到 appliance initramfs：$initramfs"

  local qemu_bin machine
  case "$arch" in
    x86_64) qemu_bin="qemu-system-x86_64"; machine="q35" ;;
    aarch64) qemu_bin="qemu-system-aarch64"; machine="virt" ;;
  esac
  require_cmd "$qemu_bin"
  require_cmd od
  require_cmd zcat

  WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/chefer-appliance-smoke.XXXXXXXX")"
  trap cleanup EXIT

  check_initramfs_busybox_arch "$initramfs" "$arch" "$WORK_DIR"

  # 同架構且 /dev/kvm 可用才用 KVM；跨架構（x86_64 host 驗 aarch64）一律 TCG 模擬。
  local accel="tcg"
  local cpu="max"
  if [[ "$(normalize_arch "$(uname -m)")" == "$arch" && -r /dev/kvm && -w /dev/kvm ]]; then
    accel="kvm"
    cpu="host"
  fi

  local console_log="$WORK_DIR/console.log"
  local stderr_log="$WORK_DIR/qemu.stderr"
  : >"$console_log"

  # 刻意不給 chefer.bundle_dir：init 的契約是 fail(125) → CHEFER_GUEST_EXIT=125 後 poweroff。
  # panic=-1 + -no-reboot：kernel panic（如 busybox 錯架構 → exec /init ENOEXEC）時 QEMU
  # 直接結束，讓下面的輪詢立刻失敗，而非等 timeout。不加 quiet：失敗時 kernel log 可診斷。
  note "裸開機 smoke（${arch}, ${qemu_bin}, accel=${accel}）：預期 console 出現 CHEFER_GUEST_EXIT=125"
  "$qemu_bin" \
    -machine "${machine},accel=${accel}" \
    -cpu "$cpu" \
    -m 512M \
    -smp 2 \
    -kernel "$kernel" \
    -initrd "$initramfs" \
    -append "console=hvc0 panic=-1" \
    -nodefaults \
    -display none \
    -no-reboot \
    -monitor none \
    -parallel none \
    -chardev "file,id=console,path=${console_log}" \
    -device virtio-serial-pci \
    -device virtconsole,chardev=console \
    >"$stderr_log" 2>&1 &
  QEMU_PID=$!

  local timeout_s="${CHEFER_SMOKE_TIMEOUT:-300}"
  local deadline=$((SECONDS + timeout_s))
  local code=""
  while true; do
    code="$(sed -n 's/.*CHEFER_GUEST_EXIT=\([0-9][0-9]*\).*/\1/p' "$console_log" | tail -n 1)"
    [[ -n "$code" ]] && break
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
      dump_logs "$console_log" "$stderr_log"
      die "QEMU 在 console 出現 CHEFER_GUEST_EXIT 前就結束了 —— 多半是 kernel panic（例如 initramfs busybox 架構錯誤 → exec /init ENOEXEC）；見上方 console dump"
    fi
    if ((SECONDS >= deadline)); then
      dump_logs "$console_log" "$stderr_log"
      die "等待 console 標記逾時（${timeout_s}s）；appliance 未在時限內開機到 chefer-init"
    fi
    sleep 1
  done

  grep -q '\[chefer-init\]' "$console_log" || {
    dump_logs "$console_log" "$stderr_log"
    die "console 沒有 [chefer-init] 標記（但有 CHEFER_GUEST_EXIT=${code}）——init 的 log 路徑異常"
  }
  if [[ "$code" != "125" ]]; then
    dump_logs "$console_log" "$stderr_log"
    die "裸開機（無 chefer.bundle_dir）預期 CHEFER_GUEST_EXIT=125，實際為 ${code}；init 的 cmdline 檢查契約可能變了，請同步更新本 smoke"
  fi

  note "boot smoke 通過（${arch}）：kernel 開機、busybox/init 可執行、[chefer-init] 與 CHEFER_GUEST_EXIT=125 標記如契約出現"
}

main "$@"
