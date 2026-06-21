"""Build a minimal cpio newc initramfs containing a single /init binary."""
import gzip
import os


def cpio_header(ino, mode, nlink, filesize, namesize, rdevmajor=0, rdevminor=0):
    """Build a 110-byte cpio 'newc' header (ASCII hex)."""
    return (
        "070701"                  # magic
        f"{ino:08X}"              # inode
        f"{mode:08X}"             # mode
        "00000000"                # uid
        "00000000"                # gid
        f"{nlink:08X}"            # nlink
        "00000000"                # mtime
        f"{filesize:08X}"         # filesize
        "00000000"                # devmajor
        "00000000"                # devminor
        f"{rdevmajor:08X}"        # rdevmajor
        f"{rdevminor:08X}"        # rdevminor
        f"{namesize:08X}"         # namesize (includes NUL)
        "00000000"                # checksum
    ).encode("ascii")


def pad4(n):
    """Bytes needed to pad n up to a 4-byte boundary."""
    return (4 - n % 4) % 4


def cpio_entry(name: str, data: bytes, mode: int, ino: int, nlink: int = 1,
               rdevmajor: int = 0, rdevminor: int = 0) -> bytes:
    name_bytes = name.encode("ascii") + b"\0"
    hdr = cpio_header(ino, mode, nlink, len(data), len(name_bytes),
                      rdevmajor, rdevminor)
    assert len(hdr) == 110
    out = hdr + name_bytes + b"\0" * pad4(110 + len(name_bytes))
    out += data + b"\0" * pad4(len(data))
    return out


def main():
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    init_path = os.path.join(
        repo_root,
        "crates", "whp-helper", "test-init", "target",
        "x86_64-unknown-linux-musl", "release", "init",
    )
    out_path = os.path.join(
        repo_root,
        "test-initramfs-real.bin",
    )

    with open(init_path, "rb") as f:
        init_data = f.read()

    archive = b""
    archive += cpio_entry(".", b"", 0o040755, 1, nlink=6)
    archive += cpio_entry("init", init_data, 0o0100755, 2)
    archive += cpio_entry("proc", b"", 0o040755, 3, nlink=2)
    archive += cpio_entry("dev", b"", 0o040755, 4, nlink=2)
    # /dev/console: char device major=5 minor=1 — kernel needs this for init's stdio
    archive += cpio_entry("dev/console", b"", 0o020666, 6, rdevmajor=5, rdevminor=1)
    archive += cpio_entry("sys", b"", 0o040755, 7, nlink=2)
    # trailer
    archive += cpio_entry("TRAILER!!!", b"", 0, 8)

    # pad to 512-byte block boundary
    if len(archive) % 512:
        archive += b"\0" * (512 - len(archive) % 512)

    # Verify magic
    assert archive[:6] == b"070701", f"Bad magic: {archive[:6]!r}"

    with gzip.open(out_path, "wb") as f:
        f.write(archive)

    # Also dump uncompressed for debugging
    raw_path = out_path.replace(".bin", "-raw.cpio")
    with open(raw_path, "wb") as f:
        f.write(archive)

    print(f"Created {out_path} ({os.path.getsize(out_path)} bytes)")
    print(f"  init binary: {len(init_data)} bytes")
    print(f"  cpio uncompressed: {len(archive)} bytes")
    print(f"  magic: {archive[:6]}")
    print(f"  raw copy: {raw_path}")


if __name__ == "__main__":
    main()
