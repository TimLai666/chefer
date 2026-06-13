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
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

valid_port() {
  [[ "$1" =~ ^[0-9]+$ ]] || return 1
  local port="$1"
  (( port >= 1 && port <= 65535 ))
}

repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd
}

is_wsl() {
  grep -qiE "microsoft|wsl" /proc/sys/kernel/osrelease /proc/version 2>/dev/null
}

pick_free_port() {
  python3 - <<'PY'
import socket

with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
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
server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
server.serve_forever()
PY
}

write_gui_image() {
  local dir="$1"
  mkdir -p "$dir"
  cat >"$dir/Dockerfile" <<'DOCKER'
FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends x11-apps x11-utils \
  && rm -rf /var/lib/apt/lists/*
CMD ["xmessage", "-title", "CheferGuiE2E", "-name", "CheferGuiE2E", "-center", "Chefer GUI E2E"]
DOCKER
}

write_wayland_image() {
  local dir="$1"
  mkdir -p "$dir"
  cat >"$dir/Dockerfile" <<'DOCKER'
FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends wayland-utils \
  && rm -rf /var/lib/apt/lists/*
CMD ["sh", "-lc", "wayland-info >/tmp/chefer-wayland-info.txt && echo CHEFER_WAYLAND_OK"]
DOCKER
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
app_version: "e2e"
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
name: LinuxE2EFail
app_version: "e2e"
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

write_gui_appcipe() {
  local path="$1"
  local data_dir="$2"
  local image_tar="$3"
  local image_platform="$4"
  mkdir -p "$(dirname "$path")"
  cat >"$path" <<YAML
version: "0.1"
name: LinuxGuiE2E
app_version: "e2e"
data_dir: "${data_dir}"
crash: fail_fast
services:
  gui:
    image:
      source: tar
      file: "${image_tar}"
      format: docker-archive
      platform: ${image_platform}
    cmd: ["xmessage", "-title", "CheferGuiE2E", "-name", "CheferGuiE2E", "-center", "Chefer GUI E2E"]
    interface_mode: gui
YAML
}

write_wayland_appcipe() {
  local path="$1"
  local data_dir="$2"
  local image_tar="$3"
  local image_platform="$4"
  mkdir -p "$(dirname "$path")"
  cat >"$path" <<YAML
version: "0.1"
name: LinuxWaylandE2E
app_version: "e2e"
data_dir: "${data_dir}"
crash: fail_fast
services:
  wayland:
    image:
      source: tar
      file: "${image_tar}"
      format: docker-archive
      platform: ${image_platform}
    cmd: ["sh", "-lc", "wayland-info >/tmp/chefer-wayland-info.txt && echo CHEFER_WAYLAND_OK"]
    interface_mode: gui
YAML
}

wait_for_http() {
  local url="$1"
  local pid="$2"
  local log="$3"
  for _ in $(seq 1 150); do
    if curl -fsS --max-time 1 "$url" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$log" >&2 || true
      die "app exited before ${url} became reachable"
    fi
    sleep 0.2
  done
  cat "$log" >&2 || true
  die "timed out waiting for ${url}"
}

assert_namespace_evidence() {
  local json="$1"
  local host_uid="$2"
  python3 - "$json" "$host_uid" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
host_uid = sys.argv[2]
uid_map = data["uid_map"].split()
if data["uid"] != 0:
    raise SystemExit(f"expected container euid 0, got {data['uid']}")
if data["pid"] != 1:
    raise SystemExit(f"expected service to run as pid 1 in its pid namespace, got {data['pid']}")
if uid_map[:3] != ["0", host_uid, "1"]:
    raise SystemExit(f"expected rootless uid_map '0 {host_uid} 1', got {data['uid_map']!r}")
PY
}

RUN_WEB_PID=""
GUI_APP_PID=""
XVFB_PID=""
WESTON_PID=""
CLEANUP_IMAGE=""
CLEANUP_GUI_IMAGE=""
CLEANUP_WAYLAND_IMAGE=""
WORK_DIR=""

run_web() {
  local artifact="$1"
  local host_port="$2"
  local log="$3"
  "$artifact" >"$log" 2>&1 &
  RUN_WEB_PID=$!
  wait_for_http "http://127.0.0.1:${host_port}/health" "$RUN_WEB_PID" "$log"
}

wait_for_x() {
  local display="$1"
  local log="$2"
  for _ in $(seq 1 100); do
    if DISPLAY="$display" xdpyinfo >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${XVFB_PID:-}" ]] && ! kill -0 "$XVFB_PID" 2>/dev/null; then
      cat "$log" >&2 || true
      die "Xvfb exited before display ${display} became reachable"
    fi
    sleep 0.1
  done
  cat "$log" >&2 || true
  die "timed out waiting for X display ${display}"
}

wait_for_wayland() {
  local runtime_dir="$1"
  local display="$2"
  local log="$3"
  local socket="${runtime_dir}/${display}"
  for _ in $(seq 1 150); do
    if [[ -S "$socket" ]] && XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$display" wayland-info >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${WESTON_PID:-}" ]] && ! kill -0 "$WESTON_PID" 2>/dev/null; then
      cat "$log" >&2 || true
      die "Weston exited before Wayland display ${display} became reachable"
    fi
    sleep 0.2
  done
  ls -la "$runtime_dir" >&2 || true
  cat "$log" >&2 || true
  die "timed out waiting for Wayland display ${display}"
}

stop_process() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -INT "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

run_gui_e2e() {
  local work="$1"
  local output_target="$2"
  local image_platform="$3"
  local cli="$4"
  local kit="$5"

  require_cmd Xvfb
  require_cmd xdpyinfo
  require_cmd xwininfo

  local display="${CHEFER_E2E_DISPLAY:-:99}"
  local xvfb_log="$work/xvfb.log"
  note "Starting Xvfb on ${display}"
  Xvfb "$display" -screen 0 1024x768x24 -ac >"$xvfb_log" 2>&1 &
  XVFB_PID=$!
  wait_for_x "$display" "$xvfb_log"
  export DISPLAY="$display"

  local gui_image="chefer-e2e-gui:$(date +%s)-$$"
  CLEANUP_GUI_IMAGE="$gui_image"
  local gui_tar="$work/images/gui.tar"
  note "Building real GUI Docker image for ${image_platform}"
  write_gui_image "$work/gui-image"
  docker build --platform "$image_platform" -t "$gui_image" "$work/gui-image"
  docker save "$gui_image" -o "$gui_tar"

  note "Building LinuxGuiE2E single-file app"
  write_gui_appcipe "$work/gui/appcipe.yml" "$work/gui-data" "$gui_tar" "$image_platform"
  "$cli" build "$work/gui/appcipe.yml" --out "$work/out-gui" --kit-dir "$kit" --target "$output_target"
  local gui_app="$work/out-gui/LinuxGuiE2E/LinuxGuiE2E_${output_target}"
  [[ -f "$gui_app" ]] || die "missing built GUI app: $gui_app"
  chmod +x "$gui_app"

  local gui_log="$work/gui.log"
  "$gui_app" >"$gui_log" 2>&1 &
  GUI_APP_PID=$!
  for _ in $(seq 1 150); do
    if DISPLAY="$display" xwininfo -root -tree 2>/dev/null | grep -q "CheferGuiE2E"; then
      note "GUI E2E window detected on ${display}"
      stop_process "$GUI_APP_PID"
      GUI_APP_PID=""
      docker image rm "$gui_image" >/dev/null 2>&1 || true
      CLEANUP_GUI_IMAGE=""
      return 0
    fi
    if ! kill -0 "$GUI_APP_PID" 2>/dev/null; then
      cat "$gui_log" >&2 || true
      die "GUI app exited before a window was detected"
    fi
    sleep 0.2
  done
  DISPLAY="$display" xwininfo -root -tree >&2 || true
  cat "$gui_log" >&2 || true
  die "timed out waiting for CheferGuiE2E window"
}

run_wayland_e2e() {
  local work="$1"
  local output_target="$2"
  local image_platform="$3"
  local cli="$4"
  local kit="$5"

  require_cmd timeout
  require_cmd wayland-info
  require_cmd weston

  local runtime_dir="$work/wayland-runtime"
  local wayland_display="${CHEFER_E2E_WAYLAND_DISPLAY:-wayland-chefer-e2e}"
  local weston_log="$work/weston.log"
  mkdir -p "$runtime_dir"
  chmod 700 "$runtime_dir"

  note "Starting headless Weston on ${wayland_display}"
  XDG_RUNTIME_DIR="$runtime_dir" weston \
    --backend=headless-backend.so \
    --socket="$wayland_display" \
    --idle-time=0 \
    --log="$weston_log" >/dev/null 2>&1 &
  WESTON_PID=$!
  wait_for_wayland "$runtime_dir" "$wayland_display" "$weston_log"
  export XDG_RUNTIME_DIR="$runtime_dir"
  export WAYLAND_DISPLAY="$wayland_display"

  local wayland_image="chefer-e2e-wayland:$(date +%s)-$$"
  CLEANUP_WAYLAND_IMAGE="$wayland_image"
  local wayland_tar="$work/images/wayland.tar"
  note "Building real Wayland Docker image for ${image_platform}"
  write_wayland_image "$work/wayland-image"
  docker build --platform "$image_platform" -t "$wayland_image" "$work/wayland-image"
  docker save "$wayland_image" -o "$wayland_tar"

  note "Building LinuxWaylandE2E single-file app"
  write_wayland_appcipe "$work/wayland/appcipe.yml" "$work/wayland-data" "$wayland_tar" "$image_platform"
  "$cli" build "$work/wayland/appcipe.yml" --out "$work/out-wayland" --kit-dir "$kit" --target "$output_target"
  local wayland_app="$work/out-wayland/LinuxWaylandE2E/LinuxWaylandE2E_${output_target}"
  [[ -f "$wayland_app" ]] || die "missing built Wayland app: $wayland_app"
  chmod +x "$wayland_app"

  local wayland_log="$work/wayland.log"
  set +e
  XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$wayland_display" timeout 30s "$wayland_app" >"$wayland_log" 2>&1
  local wayland_code=$?
  set -e
  if [[ "$wayland_code" -ne 0 ]] || ! grep -q "CHEFER_WAYLAND_OK" "$wayland_log"; then
    cat "$wayland_log" >&2 || true
    cat "$weston_log" >&2 || true
    die "Wayland E2E failed: exit=${wayland_code}, expected CHEFER_WAYLAND_OK"
  fi

  docker image rm "$wayland_image" >/dev/null 2>&1 || true
  CLEANUP_WAYLAND_IMAGE=""
  if [[ -n "${WESTON_PID:-}" ]] && kill -0 "$WESTON_PID" 2>/dev/null; then
    kill -TERM "$WESTON_PID" 2>/dev/null || true
    wait "$WESTON_PID" 2>/dev/null || true
  fi
  WESTON_PID=""
  note "Wayland E2E socket forwarding verified with headless Weston"
}

main() {
  [[ "$(uname -s)" == "Linux" ]] || die "Linux E2E must run on Linux"
  ! is_wsl || die "Linux E2E requires a native Linux host, not WSL"
  [[ "$(id -u)" != "0" ]] || die "Linux E2E must run as a non-root user to prove rootless namespaces"

  if [[ -e /proc/sys/kernel/unprivileged_userns_clone ]]; then
    [[ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" == "1" ]] \
      || die "kernel.unprivileged_userns_clone must be 1"
  fi
  if [[ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]]; then
    [[ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" != "1" ]] \
      || die "kernel.apparmor_restrict_unprivileged_userns must be 0 for this E2E"
  fi

  require_cmd cargo
  require_cmd curl
  require_cmd docker
  require_cmd python3
  require_cmd rustc

  local root
  root="$(repo_root)"
  cd "$root"

  local work
  work="$(mktemp -d "${TMPDIR:-/tmp}/chefer-e2e-linux.XXXXXXXX")"
  WORK_DIR="$work"
  local gui_summary=""
  cleanup() {
    if [[ -n "${RUN_WEB_PID:-}" ]] && kill -0 "$RUN_WEB_PID" 2>/dev/null; then
      kill -TERM "$RUN_WEB_PID" 2>/dev/null || true
      wait "$RUN_WEB_PID" 2>/dev/null || true
    fi
    stop_process "${GUI_APP_PID:-}"
    if [[ -n "${XVFB_PID:-}" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
      kill -TERM "$XVFB_PID" 2>/dev/null || true
      wait "$XVFB_PID" 2>/dev/null || true
    fi
    if [[ -n "${WESTON_PID:-}" ]] && kill -0 "$WESTON_PID" 2>/dev/null; then
      kill -TERM "$WESTON_PID" 2>/dev/null || true
      wait "$WESTON_PID" 2>/dev/null || true
    fi
    if [[ -n "${CLEANUP_IMAGE:-}" ]]; then
      docker image rm "$CLEANUP_IMAGE" >/dev/null 2>&1 || true
    fi
    if [[ -n "${CLEANUP_GUI_IMAGE:-}" ]]; then
      docker image rm "$CLEANUP_GUI_IMAGE" >/dev/null 2>&1 || true
    fi
    if [[ -n "${CLEANUP_WAYLAND_IMAGE:-}" ]]; then
      docker image rm "$CLEANUP_WAYLAND_IMAGE" >/dev/null 2>&1 || true
    fi
    if [[ "${CHEFER_E2E_KEEP:-0}" == "1" ]]; then
      echo "keeping E2E work dir: $WORK_DIR" >&2
      return
    fi
    if [[ -n "${WORK_DIR:-}" && -d "$WORK_DIR" && "$WORK_DIR" == *"/chefer-e2e-linux."* ]]; then
      rm -rf -- "$WORK_DIR"
    fi
  }
  trap cleanup EXIT

  local host_uid host_target output_target host_port guest_port base_image image image_tar cli kit
  local image_platform runtime_src runtime_name
  host_uid="$(id -u)"
  host_target="$(rustc -vV | sed -n 's/^host: //p')"
  case "$(uname -m)" in
    x86_64|amd64)
      image_platform="linux/amd64"
      output_target="${CHEFER_E2E_OUTPUT_TARGET:-x86_64-unknown-linux-musl}"
      ;;
    aarch64|arm64)
      image_platform="linux/arm64"
      output_target="${CHEFER_E2E_OUTPUT_TARGET:-aarch64-unknown-linux-musl}"
      ;;
    *) die "unsupported Linux E2E host architecture: $(uname -m)" ;;
  esac
  case "$output_target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) die "CHEFER_E2E_OUTPUT_TARGET must be a Linux musl target, got: ${output_target}" ;;
  esac
  guest_port="${CHEFER_E2E_GUEST_PORT:-8080}"
  valid_port "$guest_port" || die "invalid guest port: ${guest_port}"
  if [[ -n "${CHEFER_E2E_HOST_PORT:-}" ]]; then
    host_port="$CHEFER_E2E_HOST_PORT"
    valid_port "$host_port" || die "invalid host port: ${host_port}"
    [[ "$host_port" != "$guest_port" ]] || die "CHEFER_E2E_HOST_PORT must differ from guest port ${guest_port}"
  else
    host_port="$(pick_free_port)"
    while [[ "$host_port" == "$guest_port" ]]; do
      host_port="$(pick_free_port)"
    done
  fi
  base_image="${CHEFER_E2E_BASE_IMAGE:-python:3.12-alpine}"
  image="chefer-e2e-web:$(date +%s)-$$"
  CLEANUP_IMAGE="$image"
  image_tar="$work/images/web.tar"
  cli="${CHEFER_CLI:-$root/target/debug/chefer-cli}"
  kit="$work/kit"

  note "Building Chefer CLI for host target ${host_target}"
  cargo build -p chefer-cli
  note "Building Chefer runtime for E2E output target ${output_target}"
  cargo build -p chefer-runtime --target "$output_target"
  mkdir -p "$kit"
  runtime_src="$root/target/$output_target/debug/chefer-runtime"
  runtime_name="chefer-runtime-${output_target}"
  [[ -f "$runtime_src" ]] || die "missing built runtime: $runtime_src"
  cp "$runtime_src" "$kit/$runtime_name"

  note "Building real Docker image from ${base_image} for ${image_platform}"
  mkdir -p "$work/images"
  write_web_image "$work/web-image" "$base_image"
  docker build --platform "$image_platform" -t "$image" "$work/web-image"
  docker save "$image" -o "$image_tar"

  note "Building LinuxE2E single-file app"
  write_appcipe "$work/app/appcipe.yml" "LinuxE2E" "$work/data" "$image_tar" "$host_port" "$guest_port" "$image_platform"
  "$cli" build "$work/app/appcipe.yml" --out "$work/out" --kit-dir "$kit" --target "$output_target"
  local app="$work/out/LinuxE2E/LinuxE2E_${output_target}"
  [[ -f "$app" ]] || die "missing built app: $app"
  chmod +x "$app"

  note "Running single-file app and checking host!=guest TCP port mapping"
  local first_log first_pid ns_json
  first_log="$work/web-first.log"
  run_web "$app" "$host_port" "$first_log"
  first_pid="$RUN_WEB_PID"
  ns_json="$(curl -fsS "http://127.0.0.1:${host_port}/ns")"
  echo "namespace evidence: ${ns_json}" >&2
  assert_namespace_evidence "$ns_json" "$host_uid"
  curl -fsS "http://127.0.0.1:${host_port}/write?value=first-run" >/dev/null
  curl -fsS "http://127.0.0.1:${host_port}/shutdown" >/dev/null || true
  wait "$first_pid"
  RUN_WEB_PID=""

  note "Restarting single-file app and checking persisted data"
  local second_log second_pid persisted
  second_log="$work/web-second.log"
  run_web "$app" "$host_port" "$second_log"
  second_pid="$RUN_WEB_PID"
  persisted="$(curl -fsS "http://127.0.0.1:${host_port}/read")"
  [[ "$persisted" == "first-run" ]] || die "persisted value mismatch: expected first-run, got ${persisted}"
  curl -fsS "http://127.0.0.1:${host_port}/shutdown" >/dev/null || true
  wait "$second_pid"
  RUN_WEB_PID=""

  note "Building fail_fast app and checking exit code passthrough"
  write_fail_appcipe "$work/fail/appcipe.yml" "$work/fail-data" "$image_tar" "$image_platform"
  "$cli" build "$work/fail/appcipe.yml" --out "$work/out-fail" --kit-dir "$kit" --target "$output_target"
  local fail_app="$work/out-fail/LinuxE2EFail/LinuxE2EFail_${output_target}"
  [[ -f "$fail_app" ]] || die "missing built fail app: $fail_app"
  chmod +x "$fail_app"
  set +e
  "$fail_app" >"$work/fail.log" 2>&1
  local fail_code=$?
  set -e
  if [[ "$fail_code" -ne 42 ]]; then
    cat "$work/fail.log" >&2 || true
    die "expected fail_fast exit code 42, got ${fail_code}"
  fi

  if [[ "${CHEFER_E2E_GUI:-0}" == "1" ]]; then
    run_gui_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit"
    run_wayland_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit"
    gui_summary=", X11 GUI, and Wayland socket forwarding"
  fi

  docker image rm "$image" >/dev/null 2>&1 || true
  CLEANUP_IMAGE=""
  note "Linux E2E passed: docker save -> chefer build -> single-file run, rootless namespaces, persist, fail_fast, host!=guest port mapping${gui_summary}"
}

main "$@"
