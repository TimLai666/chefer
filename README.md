# Chefer — Cook Your Containers into Delicious Apps

For developers, **Docker** is a friendly and convenient way to package applications.
However, for end users, asking them to install Docker, pull images, configure networks, and mount volumes just to run an app is simply unrealistic.

**Chefer** was built to solve this pain point.
It combines multiple **Docker images** in a **Docker Compose-like** way (which we call an "**AppCipe**"),
then packages them into a **single standalone executable** that runs **without Docker or any container engine** — users just download and run.

> Chefer turns container app delivery from **"Please install Docker"** into **"Just double-click and run."**

With a simple **AppCipe recipe** (`appcipe.yml`),
you can "cook" your containerized application into a portable single-file app, making container technology truly zero-barrier for end users.

## Platform Support

`chefer build` runs on all three host OSes and can cross-package for any of the six output targets given a kit. What follows is **how a packaged app behaves at run time**, per host OS, feature by feature. Status reflects the current state of the project — see [Known Limitations](#known-limitations) for the caveat behind every ⚠️.

| Capability | Linux | Windows | macOS |
|---|---|---|---|
| **Run the single-file app** | ✅ rootless user namespaces, zero dependencies | ✅ WSL2 — a minimal Chefer-dedicated distro is auto-provisioned on first run | 🔧 Virtualization.framework micro-VM; guest path validated on Linux+QEMU, real-Mac VZ boot pending |
| **Backend** | `namespaces` (in-process guest-agent) | `wsl2` (bundle-embedded musl guest-agent) | `vz` (Linux appliance + musl guest-agent) |
| **Multi-service apps** (`depends_on` topo start order) | ✅ | ✅ | 🔧 |
| **Data persistence** (`persist_path` → host dir, survives restarts) | ✅ | ✅ verified | 🔧 |
| **Internal networking** (services reach each other via `127.0.0.1:<port>`) | ✅ | ✅ | 🔧 |
| **Host port mapping — TCP** (`"host:guest"`, host≠guest proxied) | ✅ | ✅ verified | 🔧 |
| **Host port mapping — UDP** | ✅ | ✅ verified — Chefer relays via the VM IP + an in-VM eth0→loopback bridge (WSL2's own forwarding is TCP-only) | 🔧 |
| **GUI services** | ✅ X11 / Wayland socket passthrough | ✅ verified via WSLg ([gui-demo](examples/gui-demo)) | 🔧 |
| **`crash: fail_fast`** (any non-zero exit tears down the app, code propagated) | ✅ | ✅ verified | 🔧 |
| **Data-dir migration** (`old_names`) | ✅ | ✅ | 🔧 |
| **Official chown/gosu images** (redis, postgres, …) | ✅ as root (no userns); rootless → single-uid | ✅ verified — distro runs as real root, runs official redis/postgres as-is | 🔧 |
| **`windows/*` containers** | ❌ | ❌ | ❌ |

Legend: ✅ implemented & exercised in CI / on a real machine · 🔧 implemented, guest path QEMU-verified, awaiting real-Apple-Silicon validation · ⚠️ works with a documented caveat · ❌ not supported.

Linux containers only — `linux/amd64` and `linux/arm64`. `windows/*` images are rejected at validation on every host.

## Skills — teach your AI agent to write AppCipes

Writing an `appcipe.yml` by hand means knowing the field reference, the validation rules, and a handful of real-world gotchas (which images need full uid mapping, how internal networking actually works in v1, what `app_version` does and doesn't do). This repo ships an **agent skill** that packs all of that into one place: [`skills/write-appcipe`](skills/write-appcipe/SKILL.md).

If you use an agentic coding tool (Claude Code, Codex, etc.), install the skill so your agent can author and validate recipes for you:

```bash
npx skills add TimLai666/chefer/skills
```

Then just ask your agent to "write an appcipe for these images" — it will follow the field reference, apply the validation rules, sidestep the known gotchas, and run `chefer check` for you.

## Install

> **Running** a Chefer-packaged app needs nothing — the single file is self-contained. You only need to install Chefer if you want to **package** apps yourself.

### Option 1 — Prebuilt binary (recommended)

Download the package for your platform from the [latest release](https://github.com/TimLai666/chefer/releases/latest) — `chefer_<version>_<target>.zip` (Windows) or `chefer_<version>_<target>.tar.gz` (Linux/macOS). Inside, the `chefer` executable sits next to a `kit/` folder (the runtimes, guest-agents and macOS appliance it needs to build apps); keep them together so the kit is auto-discovered.

```powershell
# Windows (PowerShell) — extract and put chefer on PATH (keep kit\ beside chefer.exe)
Expand-Archive chefer_<version>_x86_64-pc-windows-msvc.zip -DestinationPath $env:LOCALAPPDATA\Programs\chefer
$env:Path += ";$env:LOCALAPPDATA\Programs\chefer\chefer_<version>_x86_64-pc-windows-msvc"
chefer version
```

```bash
# Linux / macOS — extract and put chefer on PATH (keep kit/ beside the binary)
tar -xzf chefer_<version>_x86_64-unknown-linux-musl.tar.gz -C ~/.local/share/chefer
ln -sf ~/.local/share/chefer/chefer_<version>_x86_64-unknown-linux-musl/chefer ~/.local/bin/chefer
chefer version
```

One package can cross-package for **every** target (its `kit/` ships all platforms' runtimes + both guest-agent arches + the macOS appliance). Update later with `chefer upgrade`. On macOS, allow the unsigned binary in System Settings → Privacy & Security on first run.

### Option 2 — From source

```bash
git clone https://github.com/TimLai666/chefer && cd chefer
cargo build --release -p chefer-cli          # binary at target/release/chefer-cli
```

`init` / `check` / `inspect` work straight away. To `chefer build` / `run` you also need a **kit** (prebuilt runtime + guest-agent, plus the appliance for macOS targets) — either copy the `kit/` from a release, or build your own (see [Building from Source](#building-from-source)) and point at it with `--kit-dir` / `CHEFER_KIT_DIR`. From a source checkout you can also just run `cargo run -p chefer-cli -- <command>`.

## Quick Start

1. **Export your image(s) as tar** (both `docker save` and OCI archives are accepted; multi-arch archives are auto-resolved by each service's `platform`):

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

## Demos

**App + database in one file** — [examples/demo](examples/demo) is a minimal but complete two-service app: a Python HTTP service that increments a visit counter stored in **redis**, with the redis data persisted across restarts. It demonstrates internal networking (`app` reaches `db` over `127.0.0.1:6379`), opt-in persistence (`persist_path: /data`), and host port mapping (`18080:8080`) — all in one executable. Its [README](examples/demo/README.md) also documents honestly where v1's networking model stops short (services share one network namespace, so "db not exposed" is not yet true isolation — see [Known Limitations](#known-limitations)).

```bash
bash examples/demo/scripts/build-images.sh          # or .ps1 on Windows — needs Docker
cargo run -p chefer-cli -- build examples/demo/appcipe.yml --out dist
./dist/CheferDemo/CheferDemo_<target>                # then curl http://127.0.0.1:18080/
```

**GUI in one file** — [examples/gui-demo](examples/gui-demo) packages a graphical X11 program (**xeyes**) with `interface_mode: gui`. Run the single file and a window appears: on Linux via X11/Wayland socket passthrough, on Windows via WSLg. Because `fail_fast` tears the app down if the GUI program can't reach the display, "the window shows up and the app keeps running" is itself proof the GUI path works. See its [README](examples/gui-demo/README.md).

```bash
bash examples/gui-demo/scripts/build-images.sh      # or .ps1 on Windows — needs Docker
cargo run -p chefer-cli -- build examples/gui-demo/appcipe.yml --out dist
./dist/CheferGuiDemo/CheferGuiDemo_<target>          # a window with googly eyes appears
```

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

- `version: "0.1"` (required), `name` (required; also the output file and data-folder name), `app_version` (display-only metadata — **not** injected into containers), `data_dir`, `old_names` (automatic data-dir migration), `crash: fail_fast`.
- Each entry under `services:` has an `image` (a bare tar path, or the full `source`/`file`/`format`/`platform` form — currently `source: tar` only), plus optional `cmd`, `workdir`, `env`, `persist_path`, `ports` (`"host:guest[/proto]"`), `mounts` (`"<host_path>:<container_path>"`), `interface_mode` (`gui | terminal | both | none`) and `depends_on`.
- Persistence is opt-in per service via `persist_path`; data lives on the host under `{data_dir or platform default}/data/{service}/` and survives restarts.
- Inter-service networking is implicit: in v1 all services in an app share one network namespace, so a service reaches another at `127.0.0.1:<port>`. Only `ports:` entries get a host→guest proxy.

See [examples/appcipe.yml](examples/appcipe.yml) for the fully-commented reference, [examples/appcipe_simple.yml](examples/appcipe_simple.yml) for the minimal one, and the [`write-appcipe` skill](skills/write-appcipe/SKILL.md) for the full field reference, validation rules, and gotchas.

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

- **macOS VZ execution still needs physical-Mac validation.** Packaging on/for macOS can embed the Linux appliance and the guest path is validated on Linux+QEMU, but GitHub-hosted macOS runners cannot boot a Virtualization.framework guest, so the actual VZ boot path is unverified on real hardware. The `vz` backend reports itself unavailable until then.
- **No per-app network isolation in v1.** All services in an app share one network namespace, so a service with no `ports:` is still reachable from sibling services — and on Windows, WSL2's `wslrelay` auto-mirrors any in-VM loopback port to the Windows host, so a "db with no ports" is in practice still reachable from the host. Truly internal-only services need per-app netns isolation, a planned feature. See [examples/demo/README.md](examples/demo/README.md) for a measured demonstration.
- **UDP on Windows uses a Chefer-managed relay (works; minor caveat).** WSL2's own localhost forwarding is TCP-only, so Chefer forwards UDP to the VM's IP and runs an in-VM `eth0→loopback` bridge so even loopback-bound UDP services are reachable. The bridge binds after a short grace so services that bind `0.0.0.0` win the port (and are hit directly); a UDP service that binds `0.0.0.0` *slower* than that grace could rarely lose the port to the bridge — a clear, retryable bind error if it happens.
- **Official chown/gosu images and rootless Linux:** images whose entrypoint `chown`s to / `gosu`s a dedicated service uid (e.g. official `redis`, `postgres`) work on WSL2, the macOS VM, and native Linux **run as root** — there Chefer runs services as real root (no user namespace), so `chown`/`gosu` to any uid succeeds (verified end-to-end with the official `redis` image on WSL2). The one limited case is the native Linux **rootless** path (running the single file as a non-root user): the kernel only lets a process self-map a single uid after `unshare(CLONE_NEWUSER)`, so such images may fail there. This is the inherent rootless constraint (same as rootless Podman without `/etc/subuid` delegation); use an image that runs as container root if you need the rootless path.
- **GUI support is best-effort**: Linux passes through X11/Wayland sockets; Windows relies on WSLg.
- **Image source is `tar` only** (`docker save` or OCI archive; multi-arch archives auto-select by `platform`). `source: dockerfile` / `source: image` are not implemented yet.
- **Linux containers only** (`linux/amd64`, `linux/arm64`); `windows/*` containers are rejected at validation.
- `depends_on` controls **start order only** — there are no health checks in v1.
- At most **one** service per app may use `interface_mode: terminal` or `both`; host ports must be unique across the whole app.
- On Windows the runtime requires **WSL2** (`wsl --install` once, if not present).
