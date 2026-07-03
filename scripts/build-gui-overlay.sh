#!/usr/bin/env bash
set -Eeuo pipefail

# 建置 Chefer 的 GUI overlay（DESIGN §6「GUI overlay 打包契約」）：
# cage（kiosk Wayland compositor）+ Xwayland + Mesa(llvmpipe) + eudev 及其依賴閉包，
# 打成 rootfs subtree 的 tar.zst。appliance init 於開機時把它解到 tmpfs 根，
# guest-agent 便能對 gui/both 服務啟動 cage 提供顯示（WHP/vz 共用）。
#
# 產物：<out>/chefer-gui-overlay-<arch>.tar.zst（+ .sha256）。
# 與 build-pasta.sh 一致：Docker + Alpine（musl 基底、與 appliance busybox 相容），
# --platform 取得跨架構。以 `apk --root` 安裝到乾淨 staging，得到完整依賴閉包。

die() { echo "::error::$*" >&2; exit 1; }
note() { echo "==> $*" >&2; }

repo_root() { cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd; }

normalize_arch() {
  case "$1" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) die "不支援的 GUI overlay 架構：$1（預期 x86_64 或 aarch64）" ;;
  esac
}

docker_platform() {
  case "$1" in
    x86_64) echo "linux/amd64" ;;
    aarch64) echo "linux/arm64" ;;
    *) die "無法對應 docker platform：$1" ;;
  esac
}

usage() {
  printf '%s\n' \
'用法：scripts/build-gui-overlay.sh --arch <x86_64|aarch64> [--out <dir>]' \
'' \
'環境變數：' \
'  CHEFER_GUI_CONTAINER  建置容器映像（預設：alpine:3.20；決定 overlay 內套件版本）' >&2
}

main() {
  local arch="" out_dir=""
  local container="${CHEFER_GUI_CONTAINER:-alpine:3.20}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --arch) [[ $# -ge 2 ]] || die "--arch requires a value"; arch="$(normalize_arch "$2")"; shift 2 ;;
      --out) [[ $# -ge 2 ]] || die "--out requires a value"; out_dir="$2"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) usage; die "未知參數：$1" ;;
    esac
  done

  [[ -n "$arch" ]] || { usage; die "缺少 --arch"; }
  command -v docker >/dev/null 2>&1 || die "需要 docker 才能建置 GUI overlay"

  local root; root="$(repo_root)"
  out_dir="${out_dir:-$root/dist/kit}"
  mkdir -p "$out_dir"
  # 絕對化：Docker -v 對相對路徑會當成具名 volume（同 build-appliance.sh 的修正）。
  out_dir="$(cd "$out_dir" && pwd)"

  local platform; platform="$(docker_platform "$arch")"
  note "建置 GUI overlay（$arch）於容器 $container（$platform）"

  # 在容器內：apk --root 安裝到乾淨 staging（自動帶入依賴閉包，含 musl/busybox 基底）
  # → 精簡（文件/快取）→ tar.zst 輸出。
  # 套件選擇（缺一 cage 就起不來，實測依據見 docs/whp-virtio-roadmap.md M8）：
  #   cage              kiosk Wayland compositor（wlroots）
  #   xwayland          讓 X11 app 也能顯示（cage 內建整合）
  #   mesa-dri-gallium  llvmpipe 軟算繪（VM 無 GPU）
  #   eudev             wlroots 的 DRM/libinput 裝置探索需要 udev
  #   xkeyboard-config  libxkbcommon 載入鍵盤配置（缺了 cage 啟動即失敗）
  #   wl-clipboard      M8-e 剪貼簿同步的 guest 端工具
  #   seatd             seat 管理 daemon（Alpine 的 libseat 沒編 builtin backend，實測必要）
  docker run --rm --platform "$platform" \
    -e OUT_ARCH="$arch" \
    -v "$out_dir:/out" \
    "$container" \
    sh -eu -c '
      apk add --no-cache zstd >/dev/null
      root=/overlay
      mkdir -p "$root"
      apk add --root "$root" --initdb --no-cache \
        --keys-dir /etc/apk/keys \
        --repositories-file /etc/apk/repositories \
        cage xwayland mesa-dri-gallium eudev xkeyboard-config wl-clipboard seatd >/dev/null
      # Xwayland 的 access control 連「同 uid 的本機連線」都要求 cookie（實測 xhost 也被拒，
      # 而 wlroots 啟動 Xwayland 不帶 -auth、也無法傳額外參數）→ 以 shim 讓 cage 啟動的
      # Xwayland 一律帶 -ac 關閉 access control。安全邊界是 micro-VM 本身：VM 內只有
      # 本 app 的服務，不存在其他本機使用者。
      mv "$root/usr/bin/Xwayland" "$root/usr/bin/Xwayland.real"
      printf "#!/bin/sh\nexec /usr/bin/Xwayland.real \"\$@\" -ac\n" > "$root/usr/bin/Xwayland"
      chmod 0755 "$root/usr/bin/Xwayland"
      # 精簡：文件、apk 快取。保留 apk db（無害且便於日後查版本）。
      rm -rf "$root/usr/share/doc" "$root/usr/share/man" "$root/var/cache/apk"/*
      # overlay 會解到 appliance 的 tmpfs 根：不得帶入會蓋掉 /init 或裝置節點的內容。
      rm -rf "$root/init" "$root/dev" "$root/proc" "$root/sys"
      du -sh "$root" >&2
      ( cd "$root" && tar -cf - . ) | zstd -19 -T0 -f -o "/out/chefer-gui-overlay-${OUT_ARCH}.tar.zst"
      ls -lh "/out/chefer-gui-overlay-${OUT_ARCH}.tar.zst" >&2
    '

  [[ -f "$out_dir/chefer-gui-overlay-$arch.tar.zst" ]] \
    || die "建置後找不到產物：$out_dir/chefer-gui-overlay-$arch.tar.zst"
  ( cd "$out_dir" && sha256sum "chefer-gui-overlay-$arch.tar.zst" > "SHA256SUMS-gui-overlay-$arch" )
  note "完成：$out_dir/chefer-gui-overlay-$arch.tar.zst"
}

main "$@"
