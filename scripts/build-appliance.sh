#!/usr/bin/env bash
set -Eeuo pipefail

die() {
  echo "::error::$*" >&2
  exit 1
}

note() {
  echo "==> $*" >&2
}

usage() {
  printf '%s\n' \
'用法：scripts/build-appliance.sh --arch <x86_64|aarch64> --linux-ref <git-tag-or-ref> [--out <dir>] [--jobs <n>]' \
'' \
'建置 Chefer 的 Linux micro-VM appliance：' \
'  <out>/chefer-vmlinuz-<arch>' \
'  <out>/chefer-initramfs-<arch>' \
'' \
'必須明確指定 Linux source ref：可重現的 appliance 需要精確 kernel git tag/ref，' \
'不能猜測一個會移動的版本。' \
'' \
'環境變數：' \
'  CHEFER_LINUX_REPO   Linux git repository（預設：kernel.org stable tree）' \
'  CHEFER_LINUX_REF    等同 --linux-ref' \
'  CHEFER_APPLIANCE_CONTAINER  建置容器映像（預設：debian:bookworm-slim）' >&2
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

main() {
  local arch=""
  local out_dir=""
  local linux_ref="${CHEFER_LINUX_REF:-}"
  local jobs="${CHEFER_APPLIANCE_JOBS:-}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --arch)
        [[ $# -ge 2 ]] || die "--arch requires a value"
        arch="$(normalize_arch "$2")"
        shift 2
        ;;
      --linux-ref)
        [[ $# -ge 2 ]] || die "--linux-ref requires a value"
        linux_ref="$2"
        shift 2
        ;;
      --out)
        [[ $# -ge 2 ]] || die "--out requires a value"
        out_dir="$2"
        shift 2
        ;;
      --jobs|-j)
        [[ $# -ge 2 ]] || die "--jobs requires a value"
        jobs="$2"
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

  if [[ -z "$arch" ]]; then
    arch="$(normalize_arch "$(uname -m)")"
  fi
  [[ -n "$linux_ref" ]] || die "請用 --linux-ref 或 CHEFER_LINUX_REF 指定 Linux git tag/ref；為了可重現，script 不猜版本"

  command -v docker >/dev/null 2>&1 || die "缺少必要命令：docker"

  local root
  root="$(repo_root)"
  if [[ -z "$out_dir" ]]; then
    out_dir="$root/dist/appliance"
  fi
  mkdir -p "$out_dir"
  # 絕對化：下面以 `-v "$out_dir:/out"` 掛進容器。Docker 對**相對路徑**的 -v 來源
  # 會當成「具名 volume」而非綁定主機目錄（例如 `--out stage` 會把產物寫進名為
  # stage 的 volume，主機 ./stage 仍是空的 → release workflow 找不到檔案而失敗）。
  # 故一律轉成絕對路徑，讓任何呼叫端（含 `--out stage` 這種相對路徑）都能正確綁定。
  out_dir="$(cd "$out_dir" && pwd)"

  local linux_repo="${CHEFER_LINUX_REPO:-https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git}"
  local image="${CHEFER_APPLIANCE_CONTAINER:-debian:bookworm-slim}"
  local uid gid
  uid="$(id -u 2>/dev/null || echo 0)"
  gid="$(id -g 2>/dev/null || echo 0)"

  note "建置 Chefer appliance：arch=${arch}, linux_ref=${linux_ref}"
  docker run --rm \
    -e CHEFER_APPLIANCE_ARCH="$arch" \
    -e CHEFER_LINUX_REF="$linux_ref" \
    -e CHEFER_LINUX_REPO="$linux_repo" \
    -e CHEFER_APPLIANCE_OUT=/out \
    -e CHEFER_APPLIANCE_JOBS="$jobs" \
    -e HOST_UID="$uid" \
    -e HOST_GID="$gid" \
    -v "$root:/src:ro" \
    -v "$out_dir:/out" \
    "$image" \
    sh -lc "sed 's/\r$//' /src/scripts/appliance/build-inside-container.sh | bash"

  note "appliance 已輸出到 ${out_dir}"
}

main "$@"
