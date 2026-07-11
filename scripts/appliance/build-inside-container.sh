#!/usr/bin/env bash
set -Eeuo pipefail

die() {
  echo "::error::$*" >&2
  exit 1
}

note() {
  echo "==> $*" >&2
}

require_env() {
  local name="$1"
  [[ -n "${!name:-}" ]] || die "缺少必要環境變數：${name}"
}

enable_kernel_options() {
  local cfg="./scripts/config"

  "$cfg" --disable MODULES
  "$cfg" --enable BLK_DEV_INITRD
  "$cfg" --enable BINFMT_ELF
  "$cfg" --enable DEVTMPFS
  "$cfg" --enable DEVTMPFS_MOUNT
  "$cfg" --enable TMPFS
  "$cfg" --enable TMPFS_POSIX_ACL
  "$cfg" --enable PROC_FS
  "$cfg" --enable SYSFS
  "$cfg" --enable UNIX
  "$cfg" --enable INET
  "$cfg" --enable IP_PNP
  "$cfg" --enable IP_PNP_DHCP
  "$cfg" --enable PACKET
  "$cfg" --enable NET
  "$cfg" --enable NETDEVICES
  "$cfg" --enable BLOCK
  "$cfg" --enable BLK_DEV
  "$cfg" --enable VIRTIO
  "$cfg" --enable VIRTIO_PCI
  "$cfg" --enable VIRTIO_MMIO
  # x86 無 device tree，virtio-mmio 裝置須由 kernel cmdline 靜態註冊
  # （virtio_mmio.device=<size>@<base>:<irq>）——WHP 後端用此掛 virtio-blk/net。
  "$cfg" --enable VIRTIO_MMIO_CMDLINE_DEVICES
  "$cfg" --enable VIRTIO_BLK
  "$cfg" --enable VIRTIO_CONSOLE
  "$cfg" --enable VIRTIO_NET
  # pasta（bridge 出網 NAT）要在 app netns 內建立 tap 裝置；x86_64 defconfig 不含 TUN，
  # 缺了它 pasta 起不來、bridge 會靜默降級成 internal（只有 lo、無出網）。
  "$cfg" --enable TUN
  # GUI（M8）：virtio-gpu scanout（cage/wlroots 的 DRM/KMS backend）與 virtio-input
  # （WHP host 視窗轉入的鍵鼠 evdev 事件）。見 DESIGN §6「WHP GUI」。
  "$cfg" --enable DRM
  "$cfg" --enable DRM_VIRTIO_GPU
  "$cfg" --enable INPUT_EVDEV
  "$cfg" --enable VIRTIO_INPUT
  "$cfg" --enable DAX
  "$cfg" --enable FS_DAX
  "$cfg" --enable FUSE_FS
  "$cfg" --enable VIRTIO_FS
  "$cfg" --enable OVERLAY_FS
  # GUI overlay（M8）以 squashfs（zstd 壓縮）唯讀掛載 + overlayfs 疊上 VM 根，免每次開機
  # 解壓 200MB+。見 DESIGN §6「GUI overlay 打包契約」與 guest-agent/src/gui.rs。
  # squashfs 是 block filesystem，掛載檔案需經 loop 裝置（BLK_DEV_LOOP）；guest-agent
  # 以 loop ioctl 把 .sqfs 綁上 /dev/loopN 再掛。
  "$cfg" --enable BLK_DEV_LOOP
  "$cfg" --enable SQUASHFS
  "$cfg" --enable SQUASHFS_ZSTD
  "$cfg" --enable NAMESPACES
  "$cfg" --enable USER_NS
  "$cfg" --enable PID_NS
  "$cfg" --enable IPC_NS
  "$cfg" --enable UTS_NS
  "$cfg" --enable NET_NS
  "$cfg" --enable CGROUPS
  "$cfg" --enable SECCOMP
  "$cfg" --enable EPOLL
  "$cfg" --enable SIGNALFD
  "$cfg" --enable INOTIFY_USER
  "$cfg" --enable FHANDLE
  "$cfg" --enable MEMFD_CREATE
  "$cfg" --enable CGROUP_BPF
  "$cfg" --enable BPF_SYSCALL
  "$cfg" --enable PCI
  "$cfg" --enable VSOCKETS
  "$cfg" --enable VIRTIO_VSOCKETS
}

build_initramfs() {
  local root="$1"
  local busybox="$2"
  rm -rf "$root"
  mkdir -p "$root"/{bin,dev,proc,sys,run,tmp,mnt,dev/pts,dev/shm}
  cp "$busybox" "$root/bin/busybox"
  # Windows checkout 可能把 /init 轉成 CRLF；kernel 解析 shebang 時會把 \r
  # 當成 interpreter 路徑的一部分，導致 PID 1 以 127 直接結束。
  sed 's/\r$//' /src/scripts/appliance/init >"$root/init"
  chmod 0755 "$root/init" "$root/bin/busybox"

  # 這些 device node 讓極早期 init 也能輸出診斷；devtmpfs 掛上後會覆蓋。
  mknod -m 0600 "$root/dev/console" c 5 1
  mknod -m 0666 "$root/dev/null" c 1 3

  (
    cd "$root"
    find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -n
  )
}

main() {
  require_env CHEFER_APPLIANCE_ARCH
  require_env CHEFER_LINUX_REF
  require_env CHEFER_LINUX_REPO
  require_env CHEFER_APPLIANCE_OUT

  export DEBIAN_FRONTEND=noninteractive
  note "安裝 kernel 建置相依套件"
  apt-get update
  apt-get install -y --no-install-recommends \
    bc bison build-essential busybox-static ca-certificates cpio flex git gzip \
    libelf-dev libssl-dev pkg-config xz-utils
  if [[ "$CHEFER_APPLIANCE_ARCH" == "aarch64" ]]; then
    apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu
  fi

  mkdir -p /work "$CHEFER_APPLIANCE_OUT"

  # initramfs 內的 busybox 必須是「目標」架構——kernel 有 CROSS_COMPILE，busybox 也要跟著換。
  # 直接抄容器自己的 /bin/busybox 在交叉建置（如 x86_64 容器建 aarch64 appliance）會產出
  # exec /init 即 ENOEXEC 的開不了機 initramfs（實體 Mac 驗證時踩到）。Debian 主 archive
  # 本身就是 multiarch：加目標架構後用 apt-get download 取 busybox-static，不安裝（避免與
  # 容器架構的 busybox-static 衝突），dpkg-deb -x 解出靜態 binary 即可。
  local deb_arch busybox_path
  case "$CHEFER_APPLIANCE_ARCH" in
    x86_64) deb_arch="amd64" ;;
    aarch64) deb_arch="arm64" ;;
  esac
  if [[ "$deb_arch" == "$(dpkg --print-architecture)" ]]; then
    busybox_path="/bin/busybox"
  else
    note "交叉建置：下載 ${deb_arch} 的 busybox-static（initramfs 用）"
    dpkg --add-architecture "$deb_arch"
    apt-get update
    (cd /work && apt-get download "busybox-static:${deb_arch}")
    dpkg-deb -x /work/busybox-static_*_"${deb_arch}".deb "/work/busybox-${deb_arch}"
    busybox_path="/work/busybox-${deb_arch}/bin/busybox"
    [[ -f "$busybox_path" ]] || die "無法取得 ${deb_arch} 的 busybox-static"
  fi

  note "clone Linux ${CHEFER_LINUX_REF}（來源：${CHEFER_LINUX_REPO}）"
  git clone --depth 1 --branch "$CHEFER_LINUX_REF" "$CHEFER_LINUX_REPO" /work/linux
  cd /work/linux

  local make_arch image_path cross_compile
  case "$CHEFER_APPLIANCE_ARCH" in
    x86_64)
      make_arch="x86_64"
      image_path="arch/x86/boot/bzImage"
      cross_compile=""
      ;;
    aarch64)
      make_arch="arm64"
      image_path="arch/arm64/boot/Image"
      cross_compile="aarch64-linux-gnu-"
      ;;
    *)
      die "不支援的 appliance 架構：${CHEFER_APPLIANCE_ARCH}"
      ;;
  esac

  note "設定 Linux kernel config（${CHEFER_APPLIANCE_ARCH}）"
  make ARCH="$make_arch" CROSS_COMPILE="$cross_compile" defconfig
  enable_kernel_options
  make ARCH="$make_arch" CROSS_COMPILE="$cross_compile" olddefconfig

  local jobs="${CHEFER_APPLIANCE_JOBS:-$(nproc)}"
  note "建置 Linux kernel（${jobs} jobs）"
  make ARCH="$make_arch" CROSS_COMPILE="$cross_compile" -j "$jobs" "${image_path##*/}"

  local kernel_out="$CHEFER_APPLIANCE_OUT/chefer-vmlinuz-${CHEFER_APPLIANCE_ARCH}"
  local initramfs_out="$CHEFER_APPLIANCE_OUT/chefer-initramfs-${CHEFER_APPLIANCE_ARCH}"
  # --sparse=never：/out 可能是 virtiofs（OrbStack/Lima 的 Docker 掛 host 目錄），
  # 不支援 FALLOC_FL_PUNCH_HOLE，GNU cp 的 sparse 偵測會以 EINVAL 失敗。
  cp --sparse=never "$image_path" "$kernel_out"

  note "產生 initramfs"
  build_initramfs /work/initramfs-root "$busybox_path" >"$initramfs_out"

  local linux_commit
  linux_commit="$(git rev-parse HEAD)"
  {
    echo "arch=${CHEFER_APPLIANCE_ARCH}"
    echo "linux_repo=${CHEFER_LINUX_REPO}"
    echo "linux_ref=${CHEFER_LINUX_REF}"
    echo "linux_commit=${linux_commit}"
    echo "built_at_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$CHEFER_APPLIANCE_OUT/BUILDINFO-${CHEFER_APPLIANCE_ARCH}"

  (
    cd "$CHEFER_APPLIANCE_OUT"
    sha256sum "chefer-vmlinuz-${CHEFER_APPLIANCE_ARCH}" \
      "chefer-initramfs-${CHEFER_APPLIANCE_ARCH}" \
      "BUILDINFO-${CHEFER_APPLIANCE_ARCH}" >"SHA256SUMS-${CHEFER_APPLIANCE_ARCH}"
  )

  if [[ -n "${HOST_UID:-}" && -n "${HOST_GID:-}" ]]; then
    chown "$HOST_UID:$HOST_GID" "$CHEFER_APPLIANCE_OUT"/* || true
  fi

  note "appliance 建置完成："
  ls -lh "$kernel_out" "$initramfs_out" >&2
}

main "$@"
