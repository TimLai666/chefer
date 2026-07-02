# Chefer — Cook Your Containers into Delicious Apps

[![CI](https://github.com/TimLai666/chefer/actions/workflows/ci.yml/badge.svg)](https://github.com/TimLai666/chefer/actions/workflows/ci.yml)
[![Native Linux E2E](https://github.com/TimLai666/chefer/actions/workflows/e2e-linux.yml/badge.svg)](https://github.com/TimLai666/chefer/actions/workflows/e2e-linux.yml)
[![Security audit](https://github.com/TimLai666/chefer/actions/workflows/security.yml/badge.svg)](https://github.com/TimLai666/chefer/actions/workflows/security.yml)
[![Release](https://img.shields.io/github/v/release/TimLai666/chefer?sort=semver)](https://github.com/TimLai666/chefer/releases/latest)
[![License](https://img.shields.io/github/license/TimLai666/chefer)](LICENSE)

For developers, **Docker** is a friendly and convenient way to package applications.
However, for end users, asking them to install Docker, pull images, configure networks, and mount volumes just to run an app is simply unrealistic.

**Chefer** was built to solve this pain point.
It combines multiple **Docker images** in a **Docker Compose-like** way (which we call an "**AppCipe**"),
then packages them into a **single standalone executable** that runs **without Docker or any container engine** — users just download and run.

> Chefer turns container app delivery from **"Please install Docker"** into **"Just double-click and run."**

With a simple **AppCipe recipe** (`appcipe.yml`),
you can "cook" your containerized application into a portable single-file app, making container technology truly zero-barrier for end users.

**In one line:** write an `appcipe.yml`, point it at your images, and `chefer build` produces a single executable your users just double-click — no Docker, no Chefer, nothing to install. The packaged app's services share an internal network, persist data, and you declare which ports are proxied to the host. It runs on **Linux** and **Windows** today (Windows uses WSL2 when present, otherwise the new **WHP** backend boots a tiny Linux micro-VM via the Windows Hypervisor Platform — no WSL required); **macOS** packaging works with execution validation in progress — so it's *moving toward* "write once, run anywhere", not there on every OS yet. (Honesty note for v1: the internal network is **not yet host-isolated** on the `shared` mode — undeclared ports can still be reached from the host, see [Roadmap](#roadmap).)

## Platform Support

`chefer build` runs on all three host OSes and can cross-package for any of the six output targets given a kit. What follows is **how a packaged app behaves at run time**, per host OS, feature by feature. Status reflects the current state of the project — see [Known Limitations](#known-limitations) for the caveat behind every ⚠️.

| Capability | Linux | Windows | macOS |
|---|---|---|---|
| **Run the single-file app** | ✅ rootless user namespaces, zero dependencies | ✅ WSL2 — a minimal Chefer-dedicated distro is auto-provisioned for the run and best-effort removed when the app exits; **or ✅ WHP (no WSL needed)** — boots a bundle-embedded Linux micro-VM via the Windows Hypervisor Platform (validated on real hardware) | 🔧 Virtualization.framework micro-VM; guest path validated on Linux+QEMU, real-Mac VZ boot pending |
| **Backend** | `namespaces` (in-process guest-agent) | `wsl2` (preferred when present) → `whp` (no-WSL micro-VM; virtio-blk + virtio-net over MMIO) | `vz` (Linux appliance + musl guest-agent) |
| **Multi-service apps** (`depends_on` topo order + optional health checks) | ✅ | ✅ | 🔧 |
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

### Option 1 — One-line installer (recommended)

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/TimLai666/chefer/main/scripts/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/TimLai666/chefer/main/scripts/install.ps1 | iex
```

The script detects your OS/arch, downloads the matching package from the [latest release](https://github.com/TimLai666/chefer/releases/latest), verifies its sha256, installs to `~/.chefer` (or `%LOCALAPPDATA%\chefer`), and adds it to your `PATH`. Open a new terminal, then `chefer version`. Pin a version with `CHEFER_VERSION=<tag>` (`$env:CHEFER_VERSION` on Windows) and change the location with `CHEFER_INSTALL_DIR`. Update later with `chefer upgrade`. On macOS, allow the unsigned binary in System Settings → Privacy & Security on first run.

### Option 2 — Manual download

Grab `chefer_<version>_<target>.zip` (Windows) / `.tar.gz` (Linux/macOS) from the [latest release](https://github.com/TimLai666/chefer/releases/latest), extract it, and put the `chefer` executable on your `PATH` — keep the `kit/` folder beside it (the runtimes, guest-agents and macOS appliance it needs to build apps; it's auto-discovered at `<exe dir>/kit`). One package can cross-package for **every** target.

### Option 3 — From source

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
| `chefer doctor [--kit-dir <dir>]...` | Check whether the current machine can build/run Chefer apps; prints PASS/WARN/FAIL diagnostics with actionable fixes, including the Windows WHP fallback host preflight. |
| `chefer inspect <file>` | Show the footer, app metadata, service details, layer sizes, and embedded companion files of a Chefer single-file executable (no execution, no filesystem extraction). |
| `chefer version` | Show Chefer and environment version info. |
| `chefer upgrade [--channel stable] [--to <ver>] [--check-only]` | Self-update from GitHub Releases — replaces **both** the `chefer` binary and the whole `kit/` together (sha256-verified, with rollback), so the CLI and kit never drift apart. |
| `chefer selfrm [-y] [--clean-wsl]` | Remove Chefer itself (the `chefer` binary + its `kit/`) and clean the installer's PATH entry. Does **not** touch apps you packaged or their data. Current Windows packaged apps best-effort clean their temporary WSL distro on exit; `--clean-wsl` additionally removes old `chefer-rt-*` runtime leftovers. |

> `chefer inspect` also shows **"Packed by chefer"**, service layer sizes, ports, mounts, healthcheck presence, and embedded `agents/` / `vm/` companion files, so you can tell what a single-file app contains without extracting it.

`[path]` defaults to `./appcipe.yml`; a directory argument means `<dir>/appcipe.yml`.

## The AppCipe Format

A Docker Compose-flavored YAML. The essentials:

- `version: "0.1"` (required), `name` (required; also the output file and data-folder name), `app_version` (display-only metadata — **not** injected into containers), `data_dir`, `old_names` (automatic data-dir migration), `crash: fail_fast`.
- Each entry under `services:` has an `image` — either a **registry reference** (`image: redis:7.2-alpine`, pulled at build time; a pinned tag or `@sha256` digest is required — `latest`/untagged is rejected) or a **local image tar** (`image: ./db.tar`, from `docker save`), or the full `source`/`file`/`format`/`platform` form (`source: image | tar`) — plus optional `cmd`, `workdir`, `env`, `persist_path`, `ports` (`"host:guest[/proto]"`), `mounts` (`"<host_path>:<container_path>"`), `interface_mode` (`gui | terminal | both | none`), `depends_on`, and `healthcheck`.
- Persistence is opt-in per service via `persist_path`; data lives on the host under `{data_dir or platform default}/data/{service}/` and survives restarts.
- Inter-service networking is implicit: services in an app share the app network, so a service reaches another at `127.0.0.1:<port>`. Only `ports:` entries get a host→guest proxy. The app-level `network:` field (`bridge` default | `internal` | `shared`) controls host isolation — `bridge`/`internal` give the app its own netns so undeclared ports are unreachable from the host; `shared` is the old non-isolated behavior.

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
       Windows → WSL2 (auto-provisioned minimal distro) → WHP (no-WSL Linux micro-VM via Windows Hypervisor Platform: virtio-blk bundle/data, virtio-net TCP/UDP port forwarding, outbound NAT)
       macOS   → Virtualization.framework micro-VM (appliance/QEMU verified first; real-Mac VZ validation pending)
            │
            ▼
       guest-agent: assemble rootfs from layers (whiteouts),
                    bind persist dirs & mounts,
                    start services in depends_on order (health checks gate readiness),
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

To validate the Windows WSL2 runtime lifecycle on a Windows machine with WSL2 + Docker:

```powershell
.\scripts\windows-wsl-cleanup-e2e.ps1
```

The script builds a real Windows single-file app, runs it once with `CHEFER_KEEP_WSL_DISTRO=1` to prove the expected `chefer-rt-*` distro can be retained, unregisters it, then runs normally and verifies the runtime unregisters the distro after app exit. If that distro name already exists before the test, pass `-CleanExisting` only when you are sure it is not in use.

## Known Limitations

Honest list of what doesn't work (yet):

- **macOS VZ execution still needs physical-Mac validation.** Packaging on/for macOS can embed the Linux appliance and the guest path is validated on Linux+QEMU, but GitHub-hosted macOS runners cannot boot a Virtualization.framework guest, so the actual VZ boot path is unverified on real hardware. The `vz` backend reports itself unavailable until then.
- **Per-app network isolation is the default (`network: bridge`).** By default an app gets its own network namespace where **only the `ports:` you declare are reachable from the host** and undeclared ports are walled off — with outbound NAT (via a bundled `pasta`). Services still reach each other at `127.0.0.1:<port>` inside the app. `network: internal` is the same but with no outbound; `network: shared` opts back into the old behavior (all services share one netns, and on Windows WSL2's `wslrelay` mirrors any listening port to the host, so undeclared ports stay reachable). Validated on native Linux (amd64+arm64, rootless) and on real WSL2 (isolation **and** `bridge` outbound). See [examples/demo/README.md](examples/demo/README.md) for a measured demonstration of the `shared` gap.
- **UDP on Windows uses a Chefer-managed relay (works; minor caveat).** WSL2's own localhost forwarding is TCP-only, so Chefer forwards UDP to the VM's IP and runs an in-VM `eth0→loopback` bridge so even loopback-bound UDP services are reachable. The bridge binds after a short grace so services that bind `0.0.0.0` win the port (and are hit directly); a UDP service that binds `0.0.0.0` *slower* than that grace could rarely lose the port to the bridge — a clear, retryable bind error if it happens.
- **Official chown/gosu images and rootless Linux:** images whose entrypoint `chown`s to / `gosu`s a dedicated service uid (e.g. official `redis`, `postgres`) work on WSL2, the macOS VM, and native Linux **run as root** — there Chefer runs services as real root (no user namespace), so `chown`/`gosu` to any uid succeeds (verified end-to-end with the official `redis` image on WSL2). The one limited case is the native Linux **rootless** path (running the single file as a non-root user): the kernel only lets a process self-map a single uid after `unshare(CLONE_NEWUSER)`, so such images may fail there. This is the inherent rootless constraint (same as rootless Podman without `/etc/subuid` delegation); use an image that runs as container root if you need the rootless path.
- **GUI support is best-effort**: Linux passes through X11/Wayland sockets; Windows relies on WSLg. GUI is **software-rendered** (no GPU — see below).
- **No GPU or NPU access, and it's platform-bounded.** Today containers get **no GPU device nodes at all** (no `/dev/dri`, `/dev/nvidia*`, or WSL `/dev/dxg`), so packaged apps can't use the GPU for compute *or* hardware-accelerated rendering, and there is no path to the Apple Neural Engine / NPUs. Even once GPU passthrough lands (roadmap), it's only feasible on **native Linux** and **Windows via WSL2** (WSL2 has Microsoft's `/dev/dxg` GPU paravirtualization → CUDA/DirectML/OpenCL/Vulkan). It will **not** work on the non-WSL Windows (WHP) path — a raw Windows Hypervisor Platform micro-VM does not get GPU-PV (that stack is tied to WSL2/Sandbox/Hyper-V-with-GPU-P, not the bare WHP API), so **on Windows, GPU means the WSL2 backend**. And it will **not** work on macOS: Virtualization.framework's virtio-gpu is display-only, and the Apple GPU's compute and the Neural Engine are reachable only via Metal/CoreML on the host, never from a Linux guest. **Net: GPU is realistic only on native Linux and WSL2.**
- **Image source is a registry pull, a local `tar`, or a Dockerfile.** `image: redis:7.2-alpine` (or `source: image`) pulls from any OCI registry at build time (anonymous; public images — private auth not yet supported), selecting the right arch by `platform`; a pinned tag or `@sha256` digest is required (`latest`/untagged rejected for reproducibility). `image: ./db.tar` (or `source: tar`) uses a local `docker save` / OCI archive. `source: dockerfile` builds the image at build time using a container builder already on the **build** machine (docker/podman/nerdctl/Apple `container`, auto-detected; OrbStack & Docker Desktop are used via their `docker` CLI) — not reproducible (use `source: image` pinned to a digest if you need that), and the runtime still needs nothing.
- **Linux containers only** (`linux/amd64`, `linux/arm64`); `windows/*` containers are rejected at validation.
- `depends_on` supports **wait-until-ready health checks**: a service with a `healthcheck` (Docker-style command, run inside the container) gates its dependents until it's healthy; a service without one is treated as ready once spawned (start-order only).
- At most **one** service per app may use `interface_mode: terminal` or `both`; host ports must be unique across the whole app.
- **On Windows the runtime prefers WSL2, but no longer requires it.** When WSL2 is present the `wsl2` backend is used (`wsl --install` once if absent; WSL2 runs on the **Hyper-V hypervisor**, enabled via the lightweight "Virtual Machine Platform" feature — **not** the full Hyper-V role, so it works on Windows Home — plus hardware virtualization in BIOS). When WSL2 is absent, the **WHP backend** boots a bundle-embedded Linux micro-VM via the Windows Hypervisor Platform (needs hardware virtualization + the WHP optional feature) and runs the real bundle with no WSL — virtio-blk bundle/data, persistence, TCP/UDP port forwarding, and outbound NAT — including the default `network: bridge` mode (per-app netns via the bundled `pasta`, same as the other backends). The WHP path is **CPU-only** (no GPU; see above). `chefer doctor` preflights the WHP API + active-hypervisor state.

## Roadmap

- [x] **Host-isolated networking — `bridge` is now the default.** Omitting `network:` gives an app its own netns where **only declared `ports:` are reachable and undeclared ports are walled off from the host**, with outbound NAT via a bundled `pasta` (like Docker's default). `network: internal` is the same without outbound; `network: shared` opts back into the old "everything shares one netns / undeclared ports reachable" behavior. Validated on native Linux (amd64+arm64, rootless) in CI, **on real WSL2, and on the real-hardware WHP micro-VM — both `internal` isolation and `bridge` outbound**.
- [x] **Pull images straight from a registry** (like Docker Compose) — `image: redis:7.2-alpine` and `chefer build` fetches it (anonymous, public images), no `docker save` needed. **`latest`/untagged is rejected — a pinned tag or `@sha256` digest is required**, so a build is reproducible. (Private-registry auth still TODO.)
- [x] **Build from a Dockerfile** (`source: dockerfile`) — `chefer build` builds the image for you using a container builder already on the **build** machine (docker/podman/nerdctl, auto-detected), then ingests it through the normal pipeline. Optional `context:` and `build_args:`. The runtime still needs nothing. Not reproducible (use `source: image` pinned to a digest if you need that); if no builder is found you get an actionable error.
- [x] **Windows without WSL2 (WHP)** — boots the bundled Linux micro-VM appliance (the same kernel + initramfs + guest-agent the macOS `vz` backend uses) via the **Windows Hypervisor Platform (WHP)**, as a fallback to the `wsl2` backend. **Implemented and validated on real hardware:** virtio-over-MMIO blk (bundle/data) + net, data persistence across restarts, host↔guest TCP/UDP port forwarding, and guest outbound NAT (UDP + TCP) — **in all network modes, including the default `bridge`** (the bundled `pasta` NATs the per-app netns to the VM's eth0, then the helper NATs eth0 to the outside; declared-port forwarding and undeclared-port isolation verified on real hardware too). **Remaining:** no GUI yet. A software-emulation fallback (bundled QEMU/TCG) could run on machines with no virtualization at all, at a significant speed cost. **Trade-off:** this path is **CPU-only** — WSL2's GPU paravirtualization (`/dev/dxg`) is a Microsoft-integrated stack a raw WHP VMM can't tap, so anyone needing GPU must stay on the WSL2 backend.
- [~] **Faster app startup** — *partly done.* The single file now **caches its extracted bundle in a content-hashed dir** (keyed on the payload sha256): the first run decompresses+verifies once, and **subsequent runs skip extraction entirely** (no re-open, re-decompress, or re-hash of the payload) — the big win for large images launched repeatedly. (`--no-cache` forces fresh temp extraction.) The guest-agent's per-service rootfs assembly is cached, **decompresses layers in parallel** on first assembly (a worker pool decompresses ahead while layers are still applied strictly in order, preserving overlay/whiteout semantics — the pure-Rust musl `ruzstd` decode is CPU-bound), and **on root backends (WSL2 / macOS VM / native-root) uses overlayfs**: each layer is extracted once into its own content-addressed read-only lowerdir (shared/deduped across services and images, OCI whiteouts converted to overlay whiteouts), then mounted as an overlay with a per-run writable upper — no merge-copy, instant mount, layers shared. Rootless native Linux keeps the merged path (rootless can't create overlay whiteout devices). Windows WSL distros are now best-effort removed when the app exits, so WSL import remains a per-run cost unless `CHEFER_KEEP_WSL_DISTRO=1` is set for local debugging or repeated-launch profiling.
- [x] **`depends_on` health checks** (wait-until-ready), not just start order — a service with `healthcheck` is polled inside its container namespaces until healthy before later services start; services without one are considered ready once spawned.
- [ ] **Rootless Linux support for chown/gosu images** via `newuidmap` + `/etc/subuid` delegation (works today on the root backends: WSL2 / macOS VM / native-root).
- [ ] **macOS VZ boot validated on real Apple Silicon** — the guest path is already QEMU-verified; the host Virtualization.framework shim needs bare-metal hardware to certify. (Includes the per-app netns isolation path inside the VM, which already runs the same guest-agent code.) Note: VZ is virtualization, not emulation — guest arch must match host (Apple Silicon → `linux/arm64`, Intel → `linux/amd64`).
- [ ] **GUI apps on macOS** — a bigger lift than Linux/Windows (macOS has no native X11/Wayland and the app runs in a VM). Planned route: bundle a tiny kiosk Wayland compositor (`cage` + Xwayland) on a virtio-gpu scanout inside the appliance, shown on the macOS desktop via `VZVirtualMachineView`. Design in [docs/DESIGN.md](docs/DESIGN.md).
- [ ] **GPU access / CUDA compute** — not supported today: the container's `/dev` is a fixed allowlist (`null`/`zero`/`random`/`urandom`/`tty` + pts/shm), with no `/dev/dri`, `/dev/nvidia*`, or WSL `/dev/dxg`, so a packaged app cannot use the GPU for compute **or** hardware-accelerated rendering (GUI is software-rendered). Planned: opt-in GPU passthrough — bind the render/compute device nodes and inject **host-driver-version-matched** userspace libs (`libcuda.so` etc. must match the host kernel module, à la the NVIDIA Container Toolkit; the image's own libs won't do). Covers more than CUDA — the same passthrough enables ROCm/OpenCL/Vulkan compute and GL/Vulkan rendering and VAAPI/NVENC video, wherever the device nodes + libs exist. Feasible on **native Linux** (`/dev/nvidia*` + `/dev/dri`) and **Windows via the WSL2 backend** (`/dev/dxg` + bind host `/usr/lib/wsl/lib` → CUDA/DirectML/OpenCL/Vulkan). **Not** on the non-WSL Windows (WHP) path — a raw WHP micro-VM can't tap Microsoft's GPU paravirtualization, so GPU stays WSL2-only. **Not** on macOS — no NVIDIA on Apple, Virtualization.framework's virtio-gpu is display-only (no GPU compute exposed to the Linux guest), and the Apple Neural Engine / NPU is CoreML-only and cannot be exposed to a Linux guest at all.
