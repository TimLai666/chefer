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
'用法：CHEFER_LINUX_REF=<linux-git-tag> scripts/qemu-e2e.sh' \
'' \
'以 QEMU + virtiofs 開機 Chefer Linux appliance，執行真實 Chefer bundle，' \
'並驗證 namespaces、persist、fail_fast、host!=guest TCP forwarding。' \
'' \
'可選環境變數：' \
'  CHEFER_QEMU_APPLIANCE_DIR  使用既有 chefer-vmlinuz-<arch>/initramfs，不在腳本內建置' \
'  CHEFER_QEMU_TIMEOUT        每次 VM 開機 timeout（預設：240s）' \
'  CHEFER_QEMU_E2E_KEEP=1     保留暫存工作目錄以便除錯' >&2
}

repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd
}

normalize_arch() {
  case "$1" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) die "不支援的 QEMU E2E host 架構：$1" ;;
  esac
}

pick_free_port() {
  python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
}

find_virtiofsd() {
  for p in virtiofsd /usr/lib/qemu/virtiofsd /usr/libexec/virtiofsd; do
    if command -v "$p" >/dev/null 2>&1; then
      command -v "$p"
      return 0
    fi
    if [[ -x "$p" ]]; then
      echo "$p"
      return 0
    fi
  done
  return 1
}

write_web_image() {
  local dir="$1"
  local base_image="$2"
  mkdir -p "$dir"
  cat >"$dir/Dockerfile" <<DOCKER
FROM ${base_image}
WORKDIR /app
COPY server.py /app/server.py
CMD ["python", "/app/server.py"]
DOCKER
  cat >"$dir/server.py" <<'PY'
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

persist_file = Path("/data/value.txt")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        return

    def send_text(self, status, text):
        body = text.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self.send_text(200, "ok\n")
            return
        if parsed.path == "/ns":
            data = {
                "uid": os.geteuid(),
                "gid": os.getegid(),
                "pid": os.getpid(),
                "uid_map": Path("/proc/self/uid_map").read_text().strip(),
                "gid_map": Path("/proc/self/gid_map").read_text().strip(),
            }
            body = json.dumps(data, sort_keys=True).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if parsed.path == "/write":
            value = parse_qs(parsed.query).get("value", [""])[0]
            persist_file.parent.mkdir(parents=True, exist_ok=True)
            persist_file.write_text(value, encoding="utf-8")
            self.send_text(200, "written\n")
            return
        if parsed.path == "/read":
            if persist_file.exists():
                self.send_text(200, persist_file.read_text(encoding="utf-8"))
            else:
                self.send_text(404, "missing\n")
            return
        if parsed.path == "/shutdown":
            self.send_text(200, "bye\n")
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self.send_text(404, "not found\n")

port = int(os.environ.get("PORT", "8080"))
server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
server.serve_forever()
PY
}

write_appcipe() {
  local path="$1"
  local name="$2"
  local data_dir="$3"
  local image_tar="$4"
  local host_port="$5"
  local guest_port="$6"
  local image_platform="$7"
  mkdir -p "$(dirname "$path")"
  cat >"$path" <<YAML
version: "0.1"
name: ${name}
app_version: "qemu-e2e"
data_dir: "${data_dir}"
crash: fail_fast
services:
  web:
    image:
      source: tar
      file: "${image_tar}"
      format: docker-archive
      platform: ${image_platform}
    cmd: ["python", "/app/server.py"]
    env:
      PORT: "${guest_port}"
    persist_path: /data
    ports: ["${host_port}:${guest_port}"]
    interface_mode: none
YAML
}

write_fail_appcipe() {
  local path="$1"
  local data_dir="$2"
  local image_tar="$3"
  local image_platform="$4"
  mkdir -p "$(dirname "$path")"
  cat >"$path" <<YAML
version: "0.1"
name: QemuE2EFail
app_version: "qemu-e2e"
data_dir: "${data_dir}"
crash: fail_fast
services:
  fail:
    image:
      source: tar
      file: "${image_tar}"
      format: docker-archive
      platform: ${image_platform}
    cmd: ["python", "-c", "import sys; sys.exit(42)"]
    interface_mode: none
YAML
}

assert_namespace_evidence() {
  local json="$1"
  python3 - "$json" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
uid_map = data["uid_map"].split()
if data["uid"] != 0:
    raise SystemExit(f"預期容器 euid 為 0，實際為 {data['uid']}")
if data["pid"] != 1:
    raise SystemExit(f"預期服務在 pid namespace 內為 pid 1，實際為 {data['pid']}")
if len(uid_map) < 3 or uid_map[0] != "0":
    raise SystemExit(f"預期 uid_map 從容器 uid 0 開始，實際為 {data['uid_map']!r}")
PY
}

wait_for_socket() {
  local socket="$1"
  local log="$2"
  for _ in $(seq 1 100); do
    [[ -S "$socket" ]] && return 0
    sleep 0.05
  done
  cat "$log" >&2 || true
  die "等待 virtiofsd socket 逾時：$socket"
}

wait_for_http() {
  local url="$1"
  local qemu_pid="$2"
  local log="$3"
  # TCG（無 KVM）下 guest-agent rootfs 組裝 + 服務啟動可能數分鐘；
  # 600 × 0.5s = 300s，與 CHEFER_QEMU_TIMEOUT 預設 240s 的 VM 級 timeout 對齊
  # （VM timeout 會先殺 QEMU，下面 kill -0 檢查會捕到並報錯）。
  for _ in $(seq 1 600); do
    if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
      cat "$log" >&2 || true
      die "VM 在 ${url} 可連線前就已結束"
    fi
    sleep 0.5
  done
  cat "$log" >&2 || true
  die "等待 ${url} 逾時"
}

QEMU_PID=""
VIRTIOFSD_PIDS=()
WORK_DIR=""
CLEANUP_IMAGE=""

cleanup_vm_processes() {
  if [[ -n "${QEMU_PID:-}" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
    kill -TERM "$QEMU_PID" 2>/dev/null || true
    wait "$QEMU_PID" 2>/dev/null || true
  fi
  QEMU_PID=""
  for pid in "${VIRTIOFSD_PIDS[@]:-}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  VIRTIOFSD_PIDS=()
}

cleanup() {
  cleanup_vm_processes
  if [[ -n "${CLEANUP_IMAGE:-}" ]]; then
    docker image rm "$CLEANUP_IMAGE" >/dev/null 2>&1 || true
  fi
  if [[ "${CHEFER_QEMU_E2E_KEEP:-0}" == "1" ]]; then
    echo "保留 QEMU E2E 工作目錄：$WORK_DIR" >&2
    return
  fi
  if [[ -n "${WORK_DIR:-}" && -d "$WORK_DIR" && "$WORK_DIR" == *"/chefer-qemu-e2e."* ]]; then
    rm -rf -- "$WORK_DIR"
  fi
}

start_virtiofsd() {
  local virtiofsd="$1"
  local socket="$2"
  local shared_dir="$3"
  local log="$4"
  rm -f "$socket"
  "$virtiofsd" \
    --socket-path="$socket" \
    --shared-dir="$shared_dir" \
    --cache=never \
    --sandbox=none >"$log" 2>&1 &
  local pid=$!
  VIRTIOFSD_PIDS+=("$pid")
  wait_for_socket "$socket" "$log"
}

start_qemu() {
  local arch="$1"
  local kernel="$2"
  local initramfs="$3"
  local bundle_dir="$4"
  local data_dir="$5"
  local host_port="$6"
  local guest_port="$7"
  local console_log="$8"
  local virtiofsd="$9"
  local run_name="${10}"

  cleanup_vm_processes
  mkdir -p "$data_dir" "$WORK_DIR/sockets"

  local bundle_sock="$WORK_DIR/sockets/${run_name}-bundle.sock"
  local data_sock="$WORK_DIR/sockets/${run_name}-data.sock"
  start_virtiofsd "$virtiofsd" "$bundle_sock" "$bundle_dir" "$WORK_DIR/${run_name}-virtiofs-bundle.log"
  start_virtiofsd "$virtiofsd" "$data_sock" "$data_dir" "$WORK_DIR/${run_name}-virtiofs-data.log"

  local accel="tcg"
  local cpu="max"
  if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    accel="kvm"
    cpu="host"
  fi

  local cmdline
  cmdline="console=hvc0 quiet ip=dhcp panic=-1"
  cmdline+=" chefer.bundle_tag=chefer-bundle chefer.data_tag=chefer-data"
  cmdline+=" chefer.bundle_dir=/mnt/bundle chefer.data_dir=/mnt/data"

  local qemu_bin machine
  case "$arch" in
    x86_64)
      qemu_bin="qemu-system-x86_64"
      machine="q35,accel=${accel}"
      ;;
    aarch64)
      qemu_bin="qemu-system-aarch64"
      machine="virt,accel=${accel}"
      ;;
    *)
      die "不支援的 QEMU 架構：$arch"
      ;;
  esac

  require_cmd "$qemu_bin"
  : >"$console_log"
  note "啟動 QEMU（${arch}, accel=${accel}）：${run_name}"
  # TCG（無 KVM）下 guest-agent rootfs 組裝 + Python 容器啟動可能數分鐘
  timeout --kill-after=10s "${CHEFER_QEMU_TIMEOUT:-480s}" "$qemu_bin" \
    -machine "$machine" \
    -cpu "$cpu" \
    -m 1536M \
    -smp 2 \
    -object memory-backend-memfd,id=mem,size=1536M,share=on \
    -numa node,memdev=mem \
    -kernel "$kernel" \
    -initrd "$initramfs" \
    -append "$cmdline" \
    -nodefaults \
    -display none \
    -no-reboot \
    -monitor none \
    -parallel none \
    -chardev "file,id=console,path=${console_log}" \
    -device virtio-serial-pci \
    -device virtconsole,chardev=console \
    -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${host_port}-:${guest_port}" \
    -device virtio-net-pci,netdev=net0 \
    -chardev "socket,id=fsbundle,path=${bundle_sock}" \
    -device vhost-user-fs-pci,chardev=fsbundle,tag=chefer-bundle \
    -chardev "socket,id=fsdata,path=${data_sock}" \
    -device vhost-user-fs-pci,chardev=fsdata,tag=chefer-data \
    >"$WORK_DIR/${run_name}-qemu.stderr" 2>&1 &
  QEMU_PID=$!
}

wait_qemu_exit_code() {
  local expected="$1"
  local console_log="$2"
  local label="$3"

  [[ -n "${QEMU_PID:-}" ]] || die "內部錯誤：QEMU_PID 為空"
  wait "$QEMU_PID" || true
  QEMU_PID=""

  local code
  code="$(sed -n 's/.*CHEFER_GUEST_EXIT=\([0-9][0-9]*\).*/\1/p' "$console_log" | tail -n 1)"
  cleanup_vm_processes
  if [[ -z "$code" ]]; then
    cat "$console_log" >&2 || true
    die "${label}: 找不到 CHEFER_GUEST_EXIT 標記"
  fi
  if [[ "$code" != "$expected" ]]; then
    cat "$console_log" >&2 || true
    die "${label}: 預期 guest exit ${expected}，實際為 ${code}"
  fi
}

main() {
  if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
  fi
  [[ "$#" -eq 0 ]] || die "未知參數：$1"

  [[ "$(uname -s)" == "Linux" ]] || die "QEMU E2E 必須在 Linux 上執行"
  require_cmd bash
  require_cmd cargo
  require_cmd curl
  require_cmd docker
  require_cmd python3
  require_cmd rustc
  require_cmd timeout

  local root
  root="$(repo_root)"
  cd "$root"

  local arch
  arch="$(normalize_arch "$(uname -m)")"
  local image_platform guest_target qemu_pkg_hint
  case "$arch" in
    x86_64)
      image_platform="linux/amd64"
      guest_target="x86_64-unknown-linux-musl"
      qemu_pkg_hint="qemu-system-x86"
      ;;
    aarch64)
      image_platform="linux/arm64"
      guest_target="aarch64-unknown-linux-musl"
      qemu_pkg_hint="qemu-system-arm"
      ;;
  esac

  local virtiofsd
  virtiofsd="$(find_virtiofsd)" || die "缺少 virtiofsd；Ubuntu 可安裝套件：sudo apt-get install virtiofsd ${qemu_pkg_hint}"

  local work
  work="$(mktemp -d "${TMPDIR:-/tmp}/chefer-qemu-e2e.XXXXXXXX")"
  WORK_DIR="$work"
  trap cleanup EXIT

  local appliance_dir="${CHEFER_QEMU_APPLIANCE_DIR:-$work/appliance}"
  local kernel="$appliance_dir/chefer-vmlinuz-$arch"
  local initramfs="$appliance_dir/chefer-initramfs-$arch"
  if [[ ! -f "$kernel" || ! -f "$initramfs" ]]; then
    [[ -n "${CHEFER_LINUX_REF:-}" ]] || die "找不到 appliance，且未設定 CHEFER_LINUX_REF；請先執行 scripts/build-appliance.sh 或提供 CHEFER_QEMU_APPLIANCE_DIR"
    bash scripts/build-appliance.sh --arch "$arch" --linux-ref "$CHEFER_LINUX_REF" --out "$appliance_dir"
  fi
  [[ -f "$kernel" ]] || die "找不到 appliance kernel：$kernel"
  [[ -f "$initramfs" ]] || die "找不到 appliance initramfs：$initramfs"

  note "建置 Chefer CLI/runtime 與靜態 guest-agent（${guest_target}）"
  rustup target add "$guest_target"
  cargo build -p chefer-cli -p chefer-runtime
  cargo build --release -p guest-agent --target "$guest_target"

  local host_target runtime_src runtime_name kit cli agent_src
  host_target="$(rustc -vV | sed -n 's/^host: //p')"
  runtime_src="$root/target/debug/chefer-runtime"
  runtime_name="chefer-runtime-${host_target}"
  cli="$root/target/debug/chefer-cli"
  kit="$work/kit"
  mkdir -p "$kit"
  cp "$runtime_src" "$kit/$runtime_name"
  agent_src="$root/target/$guest_target/release/guest-agent"
  cp "$agent_src" "$kit/guest-agent-$arch"
  cp "$kernel" "$kit/chefer-vmlinuz-$arch"
  cp "$initramfs" "$kit/chefer-initramfs-$arch"

  local base_image image image_tar host_port guest_port
  base_image="${CHEFER_QEMU_E2E_BASE_IMAGE:-python:3.12-alpine}"
  image="chefer-qemu-e2e-web:$(date +%s)-$$"
  CLEANUP_IMAGE="$image"
  image_tar="$work/images/web.tar"
  mkdir -p "$work/images"
  note "建置真實 Docker image：base=${base_image}, platform=${image_platform}"
  write_web_image "$work/web-image" "$base_image"
  docker build --platform "$image_platform" -t "$image" "$work/web-image"
  docker save "$image" -o "$image_tar"

  guest_port="${CHEFER_QEMU_E2E_GUEST_PORT:-8080}"
  host_port="${CHEFER_QEMU_E2E_HOST_PORT:-$(pick_free_port)}"
  while [[ "$host_port" == "$guest_port" ]]; do
    host_port="$(pick_free_port)"
  done

  note "建置含內嵌 guest-agent 的 QemuE2E bundle"
  write_appcipe "$work/app/appcipe.yml" "QemuE2E" "$work/data" "$image_tar" "$host_port" "$guest_port" "$image_platform"
  "$cli" build "$work/app/appcipe.yml" --out "$work/out" --kit-dir "$kit" --target "$host_target"
  local bundle="$work/out/QemuE2E/bundle"
  [[ -d "$bundle" ]] || die "找不到已建置 bundle：$bundle"
  [[ -f "$bundle/agents/guest-agent-$arch" ]] || die "bundle 未內嵌 guest-agent-$arch"

  local first_log="$work/qemu-first.console.log"
  start_qemu "$arch" "$kernel" "$initramfs" "$bundle" "$work/vm-data" "$host_port" "$guest_port" "$first_log" "$virtiofsd" "first"
  wait_for_http "http://127.0.0.1:${host_port}/health" "$QEMU_PID" "$first_log"
  local ns_json
  ns_json="$(curl -fsS "http://127.0.0.1:${host_port}/ns")"
  echo "namespace 證據：${ns_json}" >&2
  assert_namespace_evidence "$ns_json"
  curl -fsS "http://127.0.0.1:${host_port}/write?value=qemu-first-run" >/dev/null
  curl -fsS "http://127.0.0.1:${host_port}/shutdown" >/dev/null || true
  wait_qemu_exit_code 0 "$first_log" "first run"

  local second_log="$work/qemu-second.console.log"
  start_qemu "$arch" "$kernel" "$initramfs" "$bundle" "$work/vm-data" "$host_port" "$guest_port" "$second_log" "$virtiofsd" "second"
  wait_for_http "http://127.0.0.1:${host_port}/health" "$QEMU_PID" "$second_log"
  local persisted
  persisted="$(curl -fsS "http://127.0.0.1:${host_port}/read")"
  [[ "$persisted" == "qemu-first-run" ]] || die "persist 值不符：預期 qemu-first-run，實際為 ${persisted}"
  curl -fsS "http://127.0.0.1:${host_port}/shutdown" >/dev/null || true
  wait_qemu_exit_code 0 "$second_log" "second run"

  note "建置 fail_fast bundle"
  write_fail_appcipe "$work/fail/appcipe.yml" "$work/fail-data" "$image_tar" "$image_platform"
  "$cli" build "$work/fail/appcipe.yml" --out "$work/out-fail" --kit-dir "$kit" --target "$host_target"
  local fail_bundle="$work/out-fail/QemuE2EFail/bundle"
  [[ -d "$fail_bundle" ]] || die "找不到 fail bundle：$fail_bundle"
  local fail_log="$work/qemu-fail.console.log"
  start_qemu "$arch" "$kernel" "$initramfs" "$fail_bundle" "$work/fail-vm-data" "$host_port" "$guest_port" "$fail_log" "$virtiofsd" "fail"
  wait_qemu_exit_code 42 "$fail_log" "fail_fast run"

  docker image rm "$image" >/dev/null 2>&1 || true
  CLEANUP_IMAGE=""
  note "QEMU E2E 通過：appliance 開機、virtiofs bundle/data、guest-agent namespaces、persist、fail_fast、host!=guest TCP forwarding"
}

main "$@"
