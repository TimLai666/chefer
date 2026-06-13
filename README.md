# Chefer — Cook Your Containers into Delicious Apps

For developers, **Docker** is a friendly and convenient way to package applications.
However, for end users, asking them to install Docker, pull images, configure networks, and mount volumes just to run an app is simply unrealistic.

**Chefer** was built to solve this pain point.
It combines multiple **Docker images** in a **Docker Compose-like** way (which we call it "**AppCipe**"),
then packages them into a **single standalone executable** that runs **without Docker or any container engine** — users just download and run.

> Chefer turns container app delivery from **"Please install Docker"** into **"Just double-click and run."**

With a simple **AppCipe recipe** (`appcipe.yml`),
you can "cook" your containerized application into a portable single-file app, making container technology truly zero-barrier for end users.

## Platform Support

| Capability | Linux | Windows | macOS |
|---|---|---|---|
| `chefer build` (package for any platform, given a kit) | ✅ | ✅ | ✅ |
| Run the single-file app (linux/amd64, linux/arm64 services) | ✅ rootless namespaces, no dependencies | ✅ WSL2 (a minimal Chefer-dedicated distro is created automatically on first run) | 🔜 appliance/QEMU path in progress; VZ still needs real-Mac validation |
| GUI services | ✅ X11 / Wayland socket passthrough | ✅ via WSLg (best-effort) | 🔜 |
| windows/amd64 containers | ❌ | ❌ | ❌ |

## Quick Start

1. **Export your image(s) as tar** (both `docker save` and OCI archives are accepted; multi-arch archives are auto-resolved):

   ```bash
   docker save -o images/app.tar myimage:latest
   ```

2. **Generate an AppCipe template**:

   ```bash
   chefer init
   ```

3. **Edit `appcipe.yml`** — set the app `name`, point each service's `image` at its tar, declare ports / env / persistence as needed (see [examples/appcipe.yml](examples/appcipe.yml) for a fully-commented reference), then validate:

   ```bash
   chefer check
   ```

4. **Build the single-file executable**:

   ```bash
   chefer build                                       # for the current platform
   chefer build --target x86_64-pc-windows-msvc       # or cross-package for another one
   ```

5. **Distribute** the file under `dist/<Name>/` — that's it. Users just download and run; no Docker, no installer.

> `chefer build` needs a **runtime kit** (prebuilt `chefer-runtime-<target>`, `guest-agent-<arch>`, and for macOS targets `chefer-vmlinuz-<arch>` / `chefer-initramfs-<arch>` appliance files). Every package on the [latest GitHub Release](https://github.com/TimLai666/chefer/releases/latest) ships a complete `kit/` containing runtimes for all 6 targets, both musl guest-agent architectures, and the macOS micro-VM appliance — download one package and you can package for every platform. Kit search order: `--kit-dir` > `CHEFER_KIT_DIR` > `<exe dir>/kit` > `<exe dir>`.

## CLI Commands

| Command | Description |
|---|---|
| `chefer init [dir]` | Generate an `appcipe.yml` template (never overwrites an existing file). |
| `chefer check [path] [--format pretty\|json\|yaml]` | Parse and validate the recipe, print a summary. |
| `chefer build [path] [--out <dir>] [--target <triple>]... [--kit-dir <dir>]... [--zstd-level N] [--no-embed-original] [--dry-run]` | Package into single-file executable(s); `--target` may be repeated, defaults to the host target. |
| `chefer run [path] [build options]` | Build for the host target, then run the artifact immediately (stdio passthrough, exit code propagated). |
| `chefer inspect <file>` | Show the footer and embedded manifest summary of a Chefer single-file executable (no execution, no extraction). |
| `chefer version` | Show Chefer and environment version info. |
| `chefer upgrade [--channel stable] [--to <ver>] [--check-only]` | Self-update from GitHub Releases. |

`[path]` defaults to `./appcipe.yml`; a directory argument means `<dir>/appcipe.yml`.

## The AppCipe Format

A Docker Compose-flavored YAML. The essentials:

- `version: "0.1"` (required), `name` (required; also the output file and data-folder name), `app_version`, `data_dir`, `old_names` (automatic data-dir migration), `crash: fail_fast`.
- Each entry under `services:` has an `image` (a bare tar path, or the full `source`/`file`/`format`/`platform` form — currently `source: tar` only), plus optional `cmd`, `workdir`, `env`, `persist_path`, `ports` (`"host:guest[/proto]"`), `mounts` (`"<host_path>:<container_path>"`), `interface_mode` (`gui | terminal | both | none`) and `depends_on`.
- Persistence is opt-in per service via `persist_path`; data lives on the host under `{data_dir or platform default}/data/{service}/` and survives restarts.

See [examples/appcipe.yml](examples/appcipe.yml) for the fully-commented reference and [examples/appcipe_simple.yml](examples/appcipe_simple.yml) for the minimal one.

## How It Works

At build time Chefer parses each image tar and stores its **original layers** (zstd-compressed) plus a `manifest.json` into a bundle, then appends `zstd(tar(bundle))` and an 80-byte footer to a prebuilt `chefer-runtime` binary — producing one executable. At run time that executable verifies and extracts the bundle, then hands it to a platform backend in which a small **guest-agent** assembles the rootfs from the layers (applying OCI whiteouts) and starts the services. The rootfs is always assembled inside a Linux environment, so symlinks, permissions and case-sensitivity stay intact even when the file was built on Windows.

```
appcipe.yml + image tars
        │
        ▼
chefer build
  ├─ pack:      image tars ──> bundle/ (zstd layers + manifest.json)
  └─ assemble:  [chefer-runtime][zstd(tar(bundle))][footer]
        │
        ▼
single executable ── user double-clicks ──> chefer-runtime
  ├─ verify sha256, extract bundle to temp
  ├─ resolve data dir (+ old_names migration), start port proxies
  └─ pick platform backend:
       Linux   → rootless namespaces (in-process)
       Windows → WSL2 (auto-provisioned minimal distro)
       macOS   → Virtualization.framework micro-VM (appliance/QEMU verified first; real-Mac VZ validation pending)
            │
            ▼
       guest-agent: assemble rootfs from layers (whiteouts),
                    bind persist dirs & mounts,
                    start services in depends_on order,
                    fail-fast supervision
```

## Building from Source

```bash
cargo build                 # whole workspace (Windows / Linux / macOS)
cargo test                  # unit + integration tests (no Docker needed)
```

To build the static guest-agent (needed in the kit when packaging for Windows/macOS targets):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build -p chefer-runtime --release
cargo build -p guest-agent --target x86_64-unknown-linux-musl --release
```

The musl targets are already configured to link with `rust-lld` (see [.cargo/config.toml](.cargo/config.toml)), so they build on any host without a musl C toolchain. To use self-built binaries as a kit, place them in a directory as `chefer-runtime-<target-triple>[.exe]` and `guest-agent-<arch>` and pass it via `--kit-dir` (or `CHEFER_KIT_DIR`).

To build and validate the macOS micro-VM appliance on Linux:

```bash
CHEFER_LINUX_REF=v6.6.32 bash scripts/build-appliance.sh --arch x86_64 --out dist/appliance
CHEFER_LINUX_REF=v6.6.32 bash scripts/qemu-e2e.sh
```

`scripts/qemu-e2e.sh` boots the appliance with QEMU + virtiofs, runs a real Chefer bundle through the musl guest-agent, and verifies namespaces, persistence, fail-fast exit code propagation, and host≠guest TCP forwarding. The actual macOS Virtualization.framework backend still must be validated on a physical Mac; GitHub-hosted macOS runners cannot boot nested VZ guests.

## Known Limitations

Honest list of what doesn't work (yet):

- **macOS VZ execution still needs physical-Mac validation.** Packaging on/for macOS can embed the Linux appliance; Linux+QEMU validates the guest path, but GitHub-hosted macOS runners cannot boot a Virtualization.framework guest.
- **UDP port mappings do not work on Windows** — WSL2's localhost forwarding is TCP-only. TCP mappings (including host≠guest proxying) work.
- **GUI support is best-effort**: Linux passes through X11/Wayland sockets; Windows relies on WSLg.
- **Image source is `tar` only** (`docker save` or OCI archive; multi-arch archives auto-select by `platform`). `source: dockerfile` / `source: image` are not implemented yet.
- **Linux containers only** (`linux/amd64`, `linux/arm64`); `windows/amd64` containers are rejected at validation.
- `depends_on` controls **start order only** — there are no health checks in v1.
- At most **one** service per app may use `interface_mode: terminal` or `both`; host ports must be unique across the whole app.
- On Windows the runtime requires **WSL2** (`wsl --install` once, if not present).
