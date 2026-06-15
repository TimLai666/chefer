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
        if parsed.path == "/egress":
            # 對外連線探針：bridge 模式（pasta NAT）應成功；internal 應失敗（無對外網路）。
            import socket

            try:
                conn = socket.create_connection(("1.1.1.1", 443), timeout=4)
                conn.close()
                self.send_text(200, "egress-ok\n")
            except Exception as exc:  # noqa: BLE001
                self.send_text(503, f"egress-fail: {exc}\n")
            return
        if parsed.path == "/shutdown":
            self.send_text(200, "bye\n")
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self.send_text(404, "not found\n")


port = int(os.environ.get("PORT", "8080"))
server = ThreadingHTTPServer(("127.0.0.1", port), Handler)

# 網路隔離 E2E 用：若設了 SECRET_PORT，再於 127.0.0.1 綁一個「未宣告」的 port。
# shared 模式下它會洩漏到 host loopback；internal/bridge 模式下應從 host 連不到。
secret = os.environ.get("SECRET_PORT")
if secret:
    secret_server = ThreadingHTTPServer(("127.0.0.1", int(secret)), Handler)
    threading.Thread(target=secret_server.serve_forever, daemon=True).start()

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
network: shared
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

write_netiso_appcipe() {
  local path="$1"
  local name="$2"
  local data_dir="$3"
  local image_tar="$4"
  local host_port="$5"
  local guest_port="$6"
  local secret_port="$7"
  local image_platform="$8"
  local network="$9"
  mkdir -p "$(dirname "$path")"
  # 只宣告 host_port:guest_port；secret_port 故意不宣告，用來驗證隔離。
  cat >"$path" <<YAML
version: "0.1"
name: ${name}
app_version: "e2e"
data_dir: "${data_dir}"
crash: fail_fast
network: ${network}
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
      SECRET_PORT: "${secret_port}"
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
network: shared
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
network: shared
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
network: shared
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

assert_reachable() {
  curl -fsS --max-time 4 "$1" >/dev/null 2>&1 || die "expected reachable but was not: $1"
}

# 連得到就算失敗（用於驗證未宣告的 port 在 internal 模式下不可達）。
assert_unreachable() {
  if curl -fsS --max-time 4 "$1" >/dev/null 2>&1; then
    die "expected UNREACHABLE (network isolation breach): $1"
  fi
}

# 驗證 /egress 探針的對外連線結果：ok（bridge 應可出網）或 fail（internal 應無對外網路）。
assert_egress() {
  local url="$1"
  local expect="$2"
  local body
  body="$(curl -sS --max-time 10 "$url" 2>/dev/null || true)"
  case "$expect" in
    ok) [[ "$body" == egress-ok* ]] || die "expected outbound egress OK, got: ${body:-<none>} (${url})" ;;
    fail) [[ "$body" == egress-fail* ]] || die "expected NO outbound egress, got: ${body:-<none>} (${url})" ;;
    *) die "assert_egress: bad expectation ${expect}" ;;
  esac
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

# 網路隔離 E2E：同一個 web image，分別以 shared 與 internal 打包執行。
#  - shared  ：未宣告的 secret port 在 host loopback 上可達（示範洩漏；驗證測試本身的靈敏度）
#  - internal：未宣告的 secret port 從 host 連不到（隔離成立）；宣告的 port 仍經 relay 可達
run_netns_iso_e2e() {
  local work="$1"
  local output_target="$2"
  local image_platform="$3"
  local cli="$4"
  local kit="$5"
  local image_tar="$6"

  local g h s
  g="$(pick_free_port)"
  h="$(pick_free_port)"
  while [[ "$h" == "$g" ]]; do h="$(pick_free_port)"; done
  s="$(pick_free_port)"
  while [[ "$s" == "$g" || "$s" == "$h" ]]; do s="$(pick_free_port)"; done

  # --- 預設模式：省略 network: 應為 bridge（驗證 spec→manifest→inspect 的預設翻轉）---
  note "Network E2E (default): omitting network: should pack as bridge"
  mkdir -p "$work/iso-default"
  cat >"$work/iso-default/appcipe.yml" <<YAML
version: "0.1"
name: LinuxNetDefault
app_version: "e2e"
data_dir: "$work/iso-default-data"
crash: fail_fast
services:
  web:
    image:
      source: tar
      file: "${image_tar}"
      format: docker-archive
      platform: ${image_platform}
    cmd: ["python", "/app/server.py"]
    env: { PORT: "${g}" }
    ports: ["${h}:${g}"]
    interface_mode: none
YAML
  "$cli" build "$work/iso-default/appcipe.yml" --out "$work/out-iso-default" --kit-dir "$kit" --target "$output_target"
  local default_app="$work/out-iso-default/LinuxNetDefault/LinuxNetDefault_${output_target}"
  [[ -f "$default_app" ]] || die "missing built default-net app: $default_app"
  # 直接檢查 bundle manifest（穩定，不受 inspect 表格寬度/CJK 換行影響）。
  local default_manifest="$work/out-iso-default/LinuxNetDefault/bundle/manifest.json"
  [[ -f "$default_manifest" ]] || die "missing default-net bundle manifest: $default_manifest"
  grep -q '"network"[[:space:]]*:[[:space:]]*"bridge"' "$default_manifest" \
    || die "default network should be bridge; manifest says: $(grep -o '"network"[^,]*' "$default_manifest")"
  note "Default network confirmed: bridge"

  # --- shared：示範未宣告的 secret port 會洩漏到 host（也驗證 assert_unreachable 有意義）---
  note "Network E2E (shared): undeclared port ${s} should LEAK to host loopback"
  write_netiso_appcipe "$work/iso-shared/appcipe.yml" "LinuxNetShared" "$work/iso-shared-data" \
    "$image_tar" "$h" "$g" "$s" "$image_platform" "shared"
  "$cli" build "$work/iso-shared/appcipe.yml" --out "$work/out-iso-shared" --kit-dir "$kit" --target "$output_target"
  local shared_app="$work/out-iso-shared/LinuxNetShared/LinuxNetShared_${output_target}"
  [[ -f "$shared_app" ]] || die "missing built shared-net app: $shared_app"
  chmod +x "$shared_app"
  run_web "$shared_app" "$h" "$work/iso-shared.log"
  local shared_pid="$RUN_WEB_PID"
  assert_reachable "http://127.0.0.1:${h}/health"   # 宣告的 port（經 runtime proxy）
  assert_reachable "http://127.0.0.1:${s}/health"   # 未宣告但 shared → 洩漏可達
  curl -fsS "http://127.0.0.1:${h}/shutdown" >/dev/null || true
  wait "$shared_pid" 2>/dev/null || true
  RUN_WEB_PID=""

  # --- internal：未宣告的 secret port 必須連不到；宣告的 port 仍可達 ---
  note "Network E2E (internal): declared ${h} reachable via relay, undeclared ${s} ISOLATED"
  write_netiso_appcipe "$work/iso-internal/appcipe.yml" "LinuxNetInternal" "$work/iso-internal-data" \
    "$image_tar" "$h" "$g" "$s" "$image_platform" "internal"
  "$cli" build "$work/iso-internal/appcipe.yml" --out "$work/out-iso-internal" --kit-dir "$kit" --target "$output_target"
  local internal_app="$work/out-iso-internal/LinuxNetInternal/LinuxNetInternal_${output_target}"
  [[ -f "$internal_app" ]] || die "missing built internal-net app: $internal_app"
  chmod +x "$internal_app"
  run_web "$internal_app" "$h" "$work/iso-internal.log"
  local internal_pid="$RUN_WEB_PID"
  assert_reachable "http://127.0.0.1:${h}/health"     # 宣告的 port → 跨 netns relay
  assert_unreachable "http://127.0.0.1:${s}/health"   # 關鍵：未宣告的 port 被隔離
  assert_egress "http://127.0.0.1:${h}/egress" fail   # internal：無對外網路
  curl -fsS "http://127.0.0.1:${h}/shutdown" >/dev/null || true
  wait "$internal_pid" 2>/dev/null || true
  RUN_WEB_PID=""

  # --- bridge 模式（需要系統有 pasta；經 PATH 由 guest-agent 找到）---
  if command -v pasta >/dev/null 2>&1; then
    note "Network E2E (bridge): declared ${h} reachable, undeclared ${s} isolated, AND outbound works via pasta"
    write_netiso_appcipe "$work/iso-bridge/appcipe.yml" "LinuxNetBridge" "$work/iso-bridge-data" \
      "$image_tar" "$h" "$g" "$s" "$image_platform" "bridge"
    "$cli" build "$work/iso-bridge/appcipe.yml" --out "$work/out-iso-bridge" --kit-dir "$kit" --target "$output_target"
    local bridge_app="$work/out-iso-bridge/LinuxNetBridge/LinuxNetBridge_${output_target}"
    [[ -f "$bridge_app" ]] || die "missing built bridge-net app: $bridge_app"
    chmod +x "$bridge_app"
    run_web "$bridge_app" "$h" "$work/iso-bridge.log"
    local bridge_pid="$RUN_WEB_PID"
    assert_reachable "http://127.0.0.1:${h}/health"     # 宣告的 port → relay
    assert_unreachable "http://127.0.0.1:${s}/health"   # 未宣告的 port 仍隔離
    assert_egress "http://127.0.0.1:${h}/egress" ok     # bridge：可出網（pasta NAT）
    curl -fsS "http://127.0.0.1:${h}/shutdown" >/dev/null || true
    wait "$bridge_pid" 2>/dev/null || true
    RUN_WEB_PID=""
    note "Bridge E2E passed: isolation holds and outbound NAT works via pasta"
  else
    note "pasta not installed; skipping bridge outbound assertion (internal isolation already verified)"
  fi

  note "Network isolation E2E passed: internal isolates undeclared port ${s} (no egress); declared ${h} reachable"
}

# Registry pull E2E：用 `source: image` 直接從 registry 拉一個小公開 image（alpine，釘版），
# 不經 docker save，打包成單檔並執行，驗證 pull → repack → assemble → run 全鏈。需要對外網路。
run_registry_pull_e2e() {
  local work="$1"
  local output_target="$2"
  local image_platform="$3"
  local cli="$4"
  local kit="$5"
  # 用「多層」官方 image（redis）而非單層 alpine：才能涵蓋 layer 順序重排
  #（oci-client 平行下載、回傳順序非 manifest 序）這條路徑——單層 image 抓不到該 bug。
  # 覆寫 cmd 讓它印標記後退出（redis-server 本身不會結束）。
  local ref="${CHEFER_E2E_REGISTRY_REF:-redis:7.2-alpine}"

  note "Registry pull E2E: multi-layer image ${ref} pulled straight from registry (no docker save)"
  mkdir -p "$work/reg"
  cat >"$work/reg/appcipe.yml" <<YAML
version: "0.1"
name: LinuxRegPull
app_version: "e2e"
data_dir: "$work/reg-data"
crash: fail_fast
network: shared
services:
  app:
    image:
      source: image
      file: "${ref}"
      platform: ${image_platform}
    cmd: ["sh", "-c", "echo CHEFER_REGISTRY_PULL_OK"]
    interface_mode: none
YAML
  "$cli" build "$work/reg/appcipe.yml" --out "$work/out-reg" --kit-dir "$kit" --target "$output_target"
  local reg_app="$work/out-reg/LinuxRegPull/LinuxRegPull_${output_target}"
  [[ -f "$reg_app" ]] || die "missing built registry-pull app: $reg_app"
  chmod +x "$reg_app"
  local reg_log="$work/reg.log"
  set +e
  "$reg_app" >"$reg_log" 2>&1
  local reg_code=$?
  set -e
  if [[ "$reg_code" -ne 0 ]] || ! grep -q "CHEFER_REGISTRY_PULL_OK" "$reg_log"; then
    cat "$reg_log" >&2 || true
    die "Registry pull E2E failed: exit=${reg_code}, expected CHEFER_REGISTRY_PULL_OK"
  fi
  note "Registry pull E2E passed: ${ref} pulled, packed, ran"
}

# depends_on healthcheck（command healthcheck）：
#  正向 — db 有 healthcheck（延遲 3s 才 listen），app depends_on db；若 gating 生效，
#         app 啟動時 db 必已就緒、能連上 → 印 CHEFER_DEPENDS_OK、整體 exit 0。
#  反向 — healthcheck 永遠失敗（test=false）→ retries 用盡 → fail_fast，整體非零退出，
#         且不會卡在 db 的 sleep 3600（gating 主動拆除）。
run_healthcheck_e2e() {
  local work="$1" output_target="$2" image_platform="$3" cli="$4" kit="$5" image_tar="$6"

  note "Healthcheck E2E (positive): depends_on waits until db healthcheck passes"
  mkdir -p "$work/hc-ok"
  cat >"$work/hc-ok/appcipe.yml" <<YAML
version: "0.1"
name: LinuxHealthDep
app_version: "e2e"
data_dir: "$work/hc-ok-data"
crash: fail_fast
network: internal
services:
  db:
    image:
      source: tar
      file: "$image_tar"
      platform: ${image_platform}
    cmd: ["python", "-c", "import socket,time\ntime.sleep(3)\ns=socket.socket()\ns.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\ns.bind(('127.0.0.1',7799))\ns.listen(1)\ns.settimeout(20)\ntry:\n    c,_=s.accept()\n    c.recv(16)\nexcept Exception:\n    pass\nprint('CHEFER_DB_DONE')"]
    interface_mode: none
    healthcheck:
      test: ["CMD", "python", "-c", "import socket,sys; sys.exit(0 if socket.socket().connect_ex(('127.0.0.1',7799))==0 else 1)"]
      interval: 1s
      timeout: 3s
      retries: 20
  app:
    image:
      source: tar
      file: "$image_tar"
      platform: ${image_platform}
    cmd: ["python", "-c", "import socket,sys\ns=socket.socket()\nr=s.connect_ex(('127.0.0.1',7799))\nif r==0:\n    s.send(b'x')\n    print('CHEFER_DEPENDS_OK')\n    sys.exit(0)\nprint('CHEFER_DEPENDS_FAIL')\nsys.exit(1)"]
    interface_mode: none
    depends_on: [db]
YAML
  "$cli" build "$work/hc-ok/appcipe.yml" --out "$work/out-hc-ok" --kit-dir "$kit" --target "$output_target"
  local ok_app="$work/out-hc-ok/LinuxHealthDep/LinuxHealthDep_${output_target}"
  [[ -f "$ok_app" ]] || die "missing built healthcheck app: $ok_app"
  chmod +x "$ok_app"
  local ok_log="$work/hc-ok.log"
  set +e
  timeout 60 "$ok_app" >"$ok_log" 2>&1
  local ok_code=$?
  set -e
  if [[ "$ok_code" -ne 0 ]] || ! grep -q "CHEFER_DEPENDS_OK" "$ok_log"; then
    cat "$ok_log" >&2 || true
    die "Healthcheck positive E2E failed: exit=${ok_code}, expected exit 0 + CHEFER_DEPENDS_OK"
  fi
  if grep -q "CHEFER_DEPENDS_FAIL" "$ok_log"; then
    cat "$ok_log" >&2 || true
    die "Healthcheck positive E2E: app ran before db was ready (gating broken)"
  fi
  note "Healthcheck positive E2E passed: app started only after db became healthy"

  note "Healthcheck E2E (negative): a never-passing healthcheck triggers fail_fast"
  mkdir -p "$work/hc-bad"
  cat >"$work/hc-bad/appcipe.yml" <<YAML
version: "0.1"
name: LinuxHealthFail
app_version: "e2e"
data_dir: "$work/hc-bad-data"
crash: fail_fast
network: internal
services:
  db:
    image:
      source: tar
      file: "$image_tar"
      platform: ${image_platform}
    cmd: ["sh", "-c", "sleep 3600"]
    interface_mode: none
    healthcheck:
      test: ["CMD", "false"]
      interval: 1s
      timeout: 2s
      retries: 3
YAML
  "$cli" build "$work/hc-bad/appcipe.yml" --out "$work/out-hc-bad" --kit-dir "$kit" --target "$output_target"
  local bad_app="$work/out-hc-bad/LinuxHealthFail/LinuxHealthFail_${output_target}"
  [[ -f "$bad_app" ]] || die "missing built healthcheck-fail app: $bad_app"
  chmod +x "$bad_app"
  local bad_log="$work/hc-bad.log" bad_code=0
  local started ended elapsed
  started="$(date +%s)"
  set +e
  timeout 60 "$bad_app" >"$bad_log" 2>&1
  bad_code=$?
  set -e
  ended="$(date +%s)"
  elapsed=$(( ended - started ))
  if [[ "$bad_code" -eq 0 || "$bad_code" -eq 124 ]]; then
    cat "$bad_log" >&2 || true
    die "Healthcheck negative E2E failed: expected non-zero fail_fast (got exit=${bad_code}; 124=timed out, meaning it hung on sleep 3600 instead of tearing down)"
  fi
  [[ "$elapsed" -lt 30 ]] || die "Healthcheck negative E2E: fail_fast took too long (${elapsed}s); gating did not tear down promptly"
  note "Healthcheck negative E2E passed: unhealthy service tore the app down (exit=${bad_code}, ${elapsed}s)"
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
    x86_64|amd64) image_platform="linux/amd64" ;;
    aarch64|arm64) image_platform="linux/arm64" ;;
    *) die "unsupported Linux E2E host architecture: $(uname -m)" ;;
  esac
  # 預設用 host 原生 gnu target：在 runner 上免 musl C 交叉工具鏈即可建置
  # （chefer-runtime 依賴 C 後端的 zstd-sys，musl 目標需 x86_64-linux-musl-gcc）。
  # 本 E2E 的目的是驗證原生 Linux 的 namespaces 執行路徑，與 C 連結方式無關；
  # 散佈用的 musl 靜態單檔由 release.yml 經 cross 另行建置，CI 亦有 musl 靜態檢查。
  output_target="${CHEFER_E2E_OUTPUT_TARGET:-$host_target}"
  case "$output_target" in
    *-unknown-linux-gnu | *-unknown-linux-musl) ;;
    *) die "CHEFER_E2E_OUTPUT_TARGET must be a Linux gnu/musl target, got: ${output_target}" ;;
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

  note "Checking network isolation (shared leak vs internal isolation)"
  run_netns_iso_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit" "$image_tar"

  note "Checking registry image pull (source: image)"
  run_registry_pull_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit"

  note "Checking depends_on healthcheck (wait-until-ready + fail_fast)"
  run_healthcheck_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit" "$image_tar"

  if [[ "${CHEFER_E2E_GUI:-0}" == "1" ]]; then
    run_gui_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit"
    run_wayland_e2e "$work" "$output_target" "$image_platform" "$cli" "$kit"
    gui_summary=", X11 GUI, and Wayland socket forwarding"
  fi

  docker image rm "$image" >/dev/null 2>&1 || true
  CLEANUP_IMAGE=""
  note "Linux E2E passed: docker save -> chefer build -> single-file run, rootless namespaces, persist, fail_fast, host!=guest port mapping, network isolation${gui_summary}"
}

main "$@"
