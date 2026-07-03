#!/usr/bin/env bash
set -Eeuo pipefail

die() {
  echo "::error::$*" >&2
  exit 1
}

usage() {
  die "usage: $0 <out-dir> <release-tag> <host-target>"
}

safe_asset_component() {
  local value="$1"
  [[ -n "$value" ]] || return 1
  [[ "$value" != "." && "$value" != ".." ]] || return 1
  [[ "$value" != *"/"* && "$value" != *"\\"* && "$value" != *":"* ]]
}

require_file() {
  local path="$1"
  [[ -f "$path" && ! -L "$path" ]] || die "missing required regular file: $path"
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

require_dir() {
  local path="$1"
  [[ -d "$path" && ! -L "$path" ]] || die "missing required directory: $path"
}

[[ "$#" -eq 3 ]] || usage

out_dir="$1"
tag="$2"
host_target="$3"

safe_asset_component "$tag" || die "release tag is not safe for asset filenames: $tag"
safe_asset_component "$host_target" || die "host target is not safe for asset filenames: $host_target"
require_dir "$out_dir"
require_cmd python3
require_cmd sha256sum

if [[ "$host_target" == *windows* ]]; then
  archive="chefer_${tag}_${host_target}.zip"
  host_cli="chefer.exe"
else
  archive="chefer_${tag}_${host_target}.tar.gz"
  host_cli="chefer"
fi

archive_path="$out_dir/$archive"
sha_path="$archive_path.sha256"
root_name="${archive%.zip}"
root_name="${root_name%.tar.gz}"

require_file "$archive_path"
require_file "$sha_path"

(
  cd "$out_dir"
  sha256sum -c "$(basename "$sha_path")"
)

extract_dir="$(mktemp -d "${TMPDIR:-/tmp}/chefer-release-kit.XXXXXXXX")"
cleanup() {
  if [[ -n "${extract_dir:-}" && -d "$extract_dir" && "$extract_dir" == *"/chefer-release-kit."* ]]; then
    rm -rf -- "$extract_dir"
  fi
}
trap cleanup EXIT

python3 - "$archive_path" "$extract_dir" <<'PY'
import pathlib
import stat
import sys
import tarfile
import zipfile

archive, out = sys.argv[1:]


def check_name(name):
    if not name or "\\" in name or ":" in name:
        raise SystemExit(f"unsafe archive path: {name!r}")
    path = pathlib.PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise SystemExit(f"unsafe archive path: {name!r}")


if archive.endswith(".zip"):
    with zipfile.ZipFile(archive) as zf:
        for info in zf.infolist():
            check_name(info.filename)
            mode = info.external_attr >> 16
            kind = stat.S_IFMT(mode)
            if kind and kind not in (stat.S_IFREG, stat.S_IFDIR):
                raise SystemExit(f"unsupported zip entry mode {mode:o}: {info.filename}")
        zf.extractall(out)
elif archive.endswith(".tar.gz"):
    with tarfile.open(archive, "r:gz") as tf:
        for member in tf.getmembers():
            check_name(member.name)
            if not (member.isfile() or member.isdir()):
                raise SystemExit(f"unsupported tar entry type: {member.name}")
        tf.extractall(out)
else:
    raise SystemExit(f"unsupported archive format: {archive}")
PY

mapfile -t top_entries < <(find "$extract_dir" -mindepth 1 -maxdepth 1 -print)
[[ "${#top_entries[@]}" -eq 1 ]] || die "archive must contain exactly one top-level directory"
root="${top_entries[0]}"
[[ "$(basename "$root")" == "$root_name" ]] || die "archive root mismatch: expected $root_name, got $(basename "$root")"

if find "$root" -type l -print -quit | grep -q .; then
  die "release kit must not contain symlinks"
fi

require_file "$root/$host_cli"
require_dir "$root/kit"

required_kit_files=(
  "chefer-runtime-x86_64-pc-windows-msvc.exe"
  "chefer-runtime-aarch64-pc-windows-msvc.exe"
  "chefer-runtime-x86_64-unknown-linux-musl"
  "chefer-runtime-aarch64-unknown-linux-musl"
  "chefer-runtime-x86_64-apple-darwin"
  "chefer-runtime-aarch64-apple-darwin"
  "guest-agent-x86_64"
  "guest-agent-aarch64"
  "pasta-x86_64"
  "pasta-aarch64"
  "chefer-gui-overlay-x86_64.tar.zst"
  "chefer-gui-overlay-aarch64.tar.zst"
  "chefer-vmlinuz-x86_64"
  "chefer-initramfs-x86_64"
  "chefer-vmlinuz-aarch64"
  "chefer-initramfs-aarch64"
  "chefer-vz-helper-x86_64"
  "chefer-vz-helper-aarch64"
  "chefer-whp-helper-x86_64.exe"
  "chefer-whp-helper-aarch64.exe"
)

for file in "${required_kit_files[@]}"; do
  require_file "$root/kit/$file"
done

echo "release kit verified: $archive"
