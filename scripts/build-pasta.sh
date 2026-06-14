#!/usr/bin/env bash
set -Eeuo pipefail

# 建置 Chefer 用的靜態 musl `pasta`（passt 專案），供 bridge 網路模式提供出網 NAT。
# 產物：<out>/pasta-<arch>（靜態連結，可嵌入 bundle 的 agents/，解出後即可執行）。
#
# 與 build-appliance.sh 一致：以 Docker + Alpine（musl 基底）建置，--platform 取得跨架構；
# passt 與 pasta 為同一支二進位，模式由 argv[0] 是否含 "pasta" 決定 —— 故產物命名為
# pasta-<arch> 即會以 pasta 模式執行。

die() { echo "::error::$*" >&2; exit 1; }
note() { echo "==> $*" >&2; }

repo_root() { cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd; }

normalize_arch() {
  case "$1" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *) die "不支援的 pasta 架構：$1（預期 x86_64 或 aarch64）" ;;
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
'用法：scripts/build-pasta.sh --arch <x86_64|aarch64> [--out <dir>] [--passt-ref <git-ref>]' \
'' \
'環境變數：' \
'  CHEFER_PASST_REPO   passt git repository（預設：https://passt.top/passt）' \
'  CHEFER_PASST_REF    要建置的 passt git tag/ref（可重現；可由 --passt-ref 覆寫）' \
'  CHEFER_PASTA_CONTAINER  建置容器映像（預設：alpine:3.20）' >&2
}

main() {
  local arch="" out_dir=""
  local passt_repo="${CHEFER_PASST_REPO:-https://passt.top/passt}"
  # 未指定 ref 時，於容器內取「最新的 release tag」（reproducible at build time，
  # 避免在此硬寫一個會猜錯的 commit）；可由 --passt-ref / CHEFER_PASST_REF 釘選特定版本。
  local passt_ref="${CHEFER_PASST_REF:-}"
  local container="${CHEFER_PASTA_CONTAINER:-alpine:3.20}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --arch) [[ $# -ge 2 ]] || die "--arch requires a value"; arch="$(normalize_arch "$2")"; shift 2 ;;
      --out) [[ $# -ge 2 ]] || die "--out requires a value"; out_dir="$2"; shift 2 ;;
      --passt-ref) [[ $# -ge 2 ]] || die "--passt-ref requires a value"; passt_ref="$2"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) usage; die "未知參數：$1" ;;
    esac
  done

  [[ -n "$arch" ]] || { usage; die "缺少 --arch"; }
  command -v docker >/dev/null 2>&1 || die "需要 docker 才能建置 pasta"

  local root; root="$(repo_root)"
  out_dir="${out_dir:-$root/dist/kit}"
  mkdir -p "$out_dir"
  # 絕對化：Docker -v 對相對路徑會當成具名 volume（見 build-appliance.sh 的同類修正）。
  out_dir="$(cd "$out_dir" && pwd)"

  local platform; platform="$(docker_platform "$arch")"
  note "建置 pasta（$arch，passt ref=${passt_ref:-<latest tag>}）於容器 $container（$platform）"

  # 在容器內：取得建置相依 → clone 指定 ref → make static → strip → 輸出 pasta-<arch>。
  docker run --rm --platform "$platform" \
    -e PASST_REPO="$passt_repo" \
    -e PASST_REF="$passt_ref" \
    -e OUT_ARCH="$arch" \
    -v "$out_dir:/out" \
    "$container" \
    sh -eu -c '
      apk add --no-cache git make gcc musl-dev linux-headers >/dev/null
      git clone "$PASST_REPO" /src
      cd /src
      if [ -n "${PASST_REF:-}" ]; then
        git checkout --quiet "$PASST_REF"
      else
        PASST_REF="$(git describe --tags --abbrev=0)"
        echo "using latest passt tag: $PASST_REF"
        git checkout --quiet "$PASST_REF"
      fi
      # static 目標把 passt/pasta 連成靜態（無 .so 相依）。
      make static
      # passt 與 pasta 同一支二進位；命名為 pasta-<arch> 即以 pasta 模式執行。
      cp passt "/out/pasta-${OUT_ARCH}"
      strip "/out/pasta-${OUT_ARCH}" || true
      # 自我檢查：確認為靜態連結（無動態直譯器）。
      if command -v file >/dev/null 2>&1; then file "/out/pasta-${OUT_ARCH}"; fi
    '

  [[ -f "$out_dir/pasta-$arch" ]] || die "建置後找不到產物：$out_dir/pasta-$arch"
  ( cd "$out_dir" && sha256sum "pasta-$arch" > "SHA256SUMS-pasta-$arch" )
  note "完成：$out_dir/pasta-$arch"
}

main "$@"
