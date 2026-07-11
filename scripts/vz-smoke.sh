#!/usr/bin/env bash
# 在**實體 Mac** 上一鍵驗證 chefer 的 macOS vz 後端——把所有「gated on real hardware」
# 的待驗項目一次跑完：VM 開機、virtiofs、guest-agent、exit code、埠轉發；--gui 再驗
# GUI 視窗/HID/剪貼簿/動態解析度（後四者為手動檢核，腳本會列清單）。
#
# GHA macOS runner 開不了 Virtualization.framework guest（巢狀虛擬化限制），所以 CI 只能
# compile-check；拿到真 Mac 的人執行本腳本即可收掉全部驗證。
#
# 需求：macOS 13+（Apple Silicon 或 Intel）、Xcode CLT（swiftc + codesign）、Rust toolchain、
#       以及含 macOS appliance 的 kit（release 附的 kit/，或自建 chefer-vmlinuz/-initramfs）。
# 用法：bash scripts/vz-smoke.sh [--kit-dir <dir>] [--gui]
#   --kit-dir 省略時依序找 $CHEFER_KIT_DIR、./kit。--gui 需要 kit 內有
#   chefer-gui-overlay-<arch>.sqfs（release kit 已附）。
set -Eeuo pipefail

die() { echo "vz-smoke: error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

[[ "$(uname -s)" == "Darwin" ]] || die "此腳本只能在 macOS 上跑（vz 後端 = Virtualization.framework）"
command -v swiftc >/dev/null || die "找不到 swiftc；請安裝 Xcode Command Line Tools"
command -v cargo >/dev/null || die "找不到 cargo；請安裝 Rust（https://rustup.rs）"
macos_major="$(sw_vers -productVersion | cut -d. -f1)"
[[ "$macos_major" -ge 13 ]] || die "需要 macOS 13+（virtiofs）；目前 $(sw_vers -productVersion)"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64) arch="x86_64" ;;
  *) die "不支援的架構：$(uname -m)" ;;
esac

kit_dir=""
gui=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --kit-dir) kit_dir="${2:?}"; shift 2 ;;
    --gui) gui=1; shift ;;
    *) die "未知參數：$1" ;;
  esac
done
kit_dir="${kit_dir:-${CHEFER_KIT_DIR:-$root/kit}}"
[[ -f "$kit_dir/chefer-vmlinuz-$arch" && -f "$kit_dir/chefer-initramfs-$arch" ]] \
  || die "kit 缺 macOS appliance（$kit_dir/chefer-vmlinuz-$arch / chefer-initramfs-$arch）；用 release 的 kit/ 或先跑 scripts/build-appliance.sh"
if [[ "$gui" == 1 && ! -f "$kit_dir/chefer-gui-overlay-$arch.sqfs" ]] \
  && ! command -v docker >/dev/null 2>&1; then
  die "--gui 需要 kit 內有 chefer-gui-overlay-$arch.sqfs（release kit 已附），或裝 Docker 讓腳本現建"
fi

work="$root/dist/vz-smoke"
mkdir -p "$work"

note "1/5 編譯 vz-helper（swiftc + virtualization entitlement 簽章）"
bash "$root/scripts/build-vz-helper.sh" --arch "$arch" "$work"
helper="$work/chefer-vz-helper-$arch"

note "2/5 編譯 chefer-cli + chefer-runtime（release）"
cargo build --release -p chefer-cli -p chefer-runtime --manifest-path "$root/Cargo.toml"

# guest-agent 走 bundle 內嵌，且 GUI 修復（WLR_LIBINPUT_NO_DEVICES 等）在 guest-agent 裡——
# **從原始碼建當前 main 的 guest-agent**，別用可能過舊的 kit binary。
# guest-agent 是純 Rust + rust-lld（.cargo/config.toml 已設 linker），macOS host 原生
# `cargo build --target <arch>-unknown-linux-musl` 即可（實體 Mac 驗證時實測），免 cross/Docker；
# 有 cross 就照 release.yml 用 cross。兩者皆不可行才退回 kit 的 guest-agent 並大聲警告。
musl_target="${arch}-unknown-linux-musl"
fresh_agent=""
if command -v cross >/dev/null 2>&1; then
  note "2b/5 cross build guest-agent（$musl_target，含當前 GUI 修復）"
  ( cd "$root" && cross build --release --target "$musl_target" -p guest-agent )
  fresh_agent="$root/target/$musl_target/release/guest-agent"
elif command -v rustup >/dev/null 2>&1 && rustup target add "$musl_target" >/dev/null 2>&1; then
  note "2b/5 原生 cargo build guest-agent（$musl_target，rust-lld，免 cross/Docker）"
  ( cd "$root" && cargo build --release --target "$musl_target" -p guest-agent )
  fresh_agent="$root/target/$musl_target/release/guest-agent"
else
  echo "vz-smoke: warning: 無 cross 也無法以 rustup 加 $musl_target target——改用 kit 的 guest-agent-$arch，" >&2
  echo "  它可能早於當前 main 的 GUI 修復（如 WLR_LIBINPUT_NO_DEVICES boot-race fix）。" >&2
fi

note "3/5 組 smoke kit（runtime + helper + appliance$([[ $gui == 1 ]] && echo ' + gui overlay')）"
smoke_kit="$work/kit"
mkdir -p "$smoke_kit"
cp "$root/target/release/chefer-runtime" "$smoke_kit/chefer-runtime-$arch-apple-darwin"
cp "$helper" "$smoke_kit/chefer-vz-helper-$arch"
cp "$kit_dir/chefer-vmlinuz-$arch" "$kit_dir/chefer-initramfs-$arch" "$smoke_kit/"
if [[ -n "$fresh_agent" && -f "$fresh_agent" ]]; then
  cp "$fresh_agent" "$smoke_kit/guest-agent-$arch"
else
  cp "$kit_dir/guest-agent-$arch" "$smoke_kit/" 2>/dev/null \
    || die "kit 缺 guest-agent-$arch（vz bundle 內嵌用；release kit 已附），且無 cross 可自建"
fi
if [[ $gui == 1 ]]; then
  # 動態解析度需要新 overlay（cage ≥0.2.0 + wlr-randr，Alpine 3.22）——舊 release kit 的
  # overlay（cage 0.1.5）沒有 wlr-output-management。有 Docker 就照當前腳本現建，
  # 否則沿用 kit 的並提醒該檢核項可能失敗。
  if command -v docker >/dev/null 2>&1; then
    note "3b/5 現建 GUI overlay（當前 build-gui-overlay.sh：cage 0.2.0 + wlr-randr）"
    bash "$root/scripts/build-gui-overlay.sh" --arch "$arch" --out "$smoke_kit"
  else
    cp "$kit_dir/chefer-gui-overlay-$arch.sqfs" "$smoke_kit/"
    echo "vz-smoke: warning: 無 docker，沿用 kit 的 GUI overlay——若它建自舊版腳本（cage 0.1.x），" >&2
    echo "  「動態解析度」檢核會失敗（console 出現 resize: giving up 或 wlr-randr not found）。" >&2
  fi
fi
[[ -f "$kit_dir/pasta-$arch" ]] && cp "$kit_dir/pasta-$arch" "$smoke_kit/" || true

platform="linux/$([[ "$arch" == aarch64 ]] && echo arm64 || echo amd64)"

note "4/5 驗證一：exit code 傳播（fail_fast 帶非零碼 → 單檔以同碼退出）"
cat > "$work/exitcode.yml" <<YML
version: "0.1"
name: vz-exit
network: internal
services:
  one:
    image:
      source: image
      file: alpine:3.20
      platform: $platform
    interface_mode: none
    cmd: ["sh", "-c", "echo VZ_SMOKE_ONESHOT; exit 7"]
YML
export CHEFER_KIT_DIR="$smoke_kit"
"$root/target/release/chefer-cli" build "$work/exitcode.yml" --out "$work/dist"
exit_exe="$work/dist/vz-exit/vz-exit_${arch}-apple-darwin"
set +e
CHEFER_VZ_EXPERIMENTAL=1 "$exit_exe" --extract-dir "$work/extract-exit" >"$work/exit.log" 2>&1
rc=$?
set -e
grep -q "CHEFER_GUEST_EXIT=" "$work/exit.log" \
  || die "console 沒出現 CHEFER_GUEST_EXIT 標記；log: $work/exit.log"
[[ "$rc" == 7 ]] || die "exit code 傳播失敗：預期 7、實得 $rc；log: $work/exit.log"
note "驗證一通過：VM 開機 ✓ CHEFER_GUEST_EXIT 標記 ✓ exit code 7 傳播 ✓"

note "驗證二：服務常駐 + TCP 埠轉發"
cat > "$work/smoke.yml" <<YML
version: "0.1"
name: vz-smoke
network: internal
services:
  web:
    image:
      source: image
      file: alpine:3.20
      platform: $platform
    interface_mode: none
    ports: ["18080:8080"]
    # alpine 基底沒有 httpd（在 busybox-extras），用 busybox nc 湊一個最小 HTTP 服務。
    cmd: ["sh", "-c", "echo VZ_SMOKE_SERVICE_UP; while true; do printf 'HTTP/1.0 200 OK\\r\\n\\r\\nok\\n' | nc -l -p 8080; done"]
YML
"$root/target/release/chefer-cli" build "$work/smoke.yml" --out "$work/dist"
exe="$work/dist/vz-smoke/vz-smoke_${arch}-apple-darwin"
[[ -x "$exe" ]] || die "build 沒產出 $exe"

log="$work/run.log"
export CHEFER_VZ_EXPERIMENTAL=1
"$exe" --extract-dir "$work/extract" >"$log" 2>&1 &
app_pid=$!
trap 'kill "$app_pid" 2>/dev/null || true' EXIT

# 等服務起來（console 直通到 log），驗 httpd 埠轉發，然後收掉。
ok_boot=0 ok_port=0
for _ in $(seq 1 120); do
  if grep -q "VZ_SMOKE_SERVICE_UP" "$log" 2>/dev/null; then ok_boot=1; break; fi
  kill -0 "$app_pid" 2>/dev/null || break
  sleep 1
done
[[ $ok_boot == 1 ]] || { tail -40 "$log" >&2; die "guest 服務沒起來（120s）；上面是 console 尾巴"; }
for _ in $(seq 1 30); do
  if curl -fsS -m 2 "http://127.0.0.1:18080/" >/dev/null 2>&1; then ok_port=1; break; fi
  sleep 1
done
kill -INT "$app_pid" 2>/dev/null || true
wait "$app_pid" 2>/dev/null || true
trap - EXIT
# 對 runtime 單發 SIGINT（非終端機 Ctrl+C 的整個 process group）不會帶走 helper/VM，
# 實測會殘留吃 CPU——收尾把本次 work dir 下的 helper 一併清掉。
pkill -f "$work/.*chefer-vz-helper" 2>/dev/null || true
grep -q "CHEFER_GUEST_IP=" "$log" || die "console 沒出現 CHEFER_GUEST_IP 標記（埠轉發不會啟動）；log: $log"
[[ $ok_port == 1 ]] || die "TCP 埠轉發失敗（curl 127.0.0.1:18080 不通）；log: $log"
note "驗證二通過：服務常駐 ✓ CHEFER_GUEST_IP 標記 ✓ TCP 轉發 ✓"

if [[ $gui == 1 ]]; then
  note "5/5 驗證三（GUI，手動檢核）：即將開 xclock 視窗（bridge 出網裝 xclock，順帶驗 pasta NAT）"
  cat > "$work/gui.yml" <<YML
version: "0.1"
name: vz-gui-smoke
network: bridge
services:
  clock:
    image:
      source: image
      file: alpine:3.20
      platform: $platform
    interface_mode: gui
    cmd: ["sh", "-c", "apk add --no-cache xclock || { echo 'apk failed - bridge outbound (pasta) not working?'; exit 1; }; xclock -update 1"]
YML
  "$root/target/release/chefer-cli" build "$work/gui.yml" --out "$work/dist"
  cat <<'CHECK'
  手動檢核清單（依 DESIGN §6 macOS GUI ③④）：
    [ ] 出現以 app 名為標題的視窗，xclock 指針在動（VZVirtualMachineView 顯示）
    [ ] 滑鼠移動/點擊、鍵盤輸入進得了 guest（HID）
    [ ] host 複製文字 → guest 內可貼上；guest 複製 → host 可貼上（剪貼簿，含 PNG 圖）
    [ ] 關窗 → app 乾淨結束、行程退出（關窗 = app 結束語意）
    [ ] 動態解析度（opt-in，macOS 14+；guest 側 re-modeset 由 guest-agent resize watcher
        提供，需上面 3b/5 的新 overlay）——本清單跑完後另跑：
          CHEFER_VZ_EXPERIMENTAL=1 CHEFER_VZ_DYNAMIC_RESOLUTION=1 \
            CHEFER_VZ_GUI_TEST_RESIZE=20:1100x700 <同一個單檔>
        20 秒後視窗自動縮放（與手動拖拉同路徑），console 應出現
        「gui: resize: Virtual-1 -> 2200x1400」（Retina 2x）且畫面重排非拉伸。
    （預設模式的拖拉=view 等比縮放、不 re-modeset——預設取捨見 DESIGN §6 ④：
      Retina 像素模式下 Linux guest 無 HiDPI output scale，UI 會縮半。）
CHECK
  CHEFER_VZ_EXPERIMENTAL=1 "$work/dist/vz-gui-smoke/vz-gui-smoke_${arch}-apple-darwin" --extract-dir "$work/extract-gui"
  note "GUI app 已結束（exit $?）——請依上方清單回填結果"
else
  note "5/5 略過 GUI（加 --gui 執行 GUI 驗證）"
fi

note "全部完成。驗證通過後請更新 docs/DESIGN.md 與 README 的「real-Mac validation pending」標記。"
