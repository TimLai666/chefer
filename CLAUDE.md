# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chefer packages multiple Docker/OCI images (described by an "AppCipe" recipe, `appcipe.yml` — a Docker Compose-like format) into a single standalone executable that runs without Docker or any container engine. The end-user just downloads and runs one file; services run inside container isolation (Linux rootless namespaces, or a Chefer-dedicated WSL2 distro on Windows; macOS execution is planned).

The architecture contract is [docs/DESIGN.md](docs/DESIGN.md) — the single source of truth for cross-crate formats, APIs and behavior. Change it first before changing anything cross-crate. Comments and docs are largely in Traditional Chinese.

## Commands

```powershell
cargo build                          # build the whole workspace
cargo build -p chefer-cli            # build one crate
cargo run -p chefer-cli -- check examples/appcipe.yml   # validate a recipe
cargo run -p chefer-cli -- build examples/appcipe.yml --dry-run
cargo test                           # run tests (cargo test -p <crate> for one crate)
cargo build -p guest-agent --target x86_64-unknown-linux-musl --release  # static agent (rust-lld preset in .cargo/config.toml)
cargo fmt
cargo clippy
```

Testing: `cargo test` needs neither Docker nor WSL — integration tests (`crates/chefer-cli/tests/cli_e2e.rs`, `crates/chefer-pack/tests/pack_tests.rs`) synthesize minimal docker-archive/OCI image tars in-test and run init/check/build/inspect against the real CLI binary. A *full* manual E2E (real image → single-file exe → actually running it) additionally requires Docker (`docker save` an image) and, on a Windows host, WSL2.

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) builds and tests on Linux/Windows/macOS and verifies the musl guest-agent links statically. Releases ([.github/workflows/release.yml](.github/workflows/release.yml)) build chefer-cli + chefer-runtime for 6 targets (and guest-agent for both musl arches), then attach one complete kit per host platform (CLI + all runtimes + both agents) to the GitHub Release.

## Architecture

Cargo workspace under `crates/`. The data flow is:

**Build time:** `appcipe.yml` → **appcipe-spec** (Serde types, parsing, validation) → **appcipe-normalize** (host path absolutization, legacy-field migration; `appcipe_normalize::load` is the one official entry point — CLI and pack always go through it) → **chefer-pack** (parses each service's image tar — docker-archive or OCI layout, multi-arch resolved by `platform` — and stores the **original layers as zstd-compressed tars** plus `manifest.json` in a bundle dir; the bundle never contains an extracted rootfs, since a Windows host can't faithfully hold a Linux rootfs) → **chefer-assembler** (copies a prebuilt `chefer-runtime`, streams `zstd(tar(bundle/))` after it, writes an 80-byte footer → the final single-file executable).

**Run time:** the single file is **chefer-runtime** (reads its own footer, stream-verifies sha256, extracts the bundle to temp, resolves the data dir + `old_names` migration, starts host≠guest port proxies) → **vmm-backend** (picks the first available `ExecBackend`: Linux `namespaces` calls guest-agent in-process; Windows `wsl2` imports a minimal Chefer-dedicated WSL distro on first run and executes the bundle-embedded musl guest-agent inside it; macOS `vz` is a skeleton that reports a clear unsupported error) → **guest-agent** (assembles the rootfs from layers applying OCI whiteouts, with caching; binds persist dirs and mounts; starts services in `depends_on` topo order; fail-fast supervision).

**chefer-bundle** is the shared protocol crate and sole definition of `manifest.json` serde types, the footer format, bundle layout helpers, `PortSpec`/`MountSpec` parsing, `topo_sort`, and **kit discovery** — other crates must import it, never hand-roll JSON/footer bytes.

**Kit:** `chefer build` needs prebuilt binaries — `chefer-runtime-<target-triple>[.exe]` per output target and `guest-agent-<arch>` (musl static; embedded into bundles for Windows/macOS targets). Search order: `--kit-dir` > `CHEFER_KIT_DIR` > `<exe dir>/kit/` > `<exe dir>`.

**chefer-cli** is the user-facing CLI (`init`, `check`, `build`, `run`, `inspect`, `version`, `upgrade`), one file per command under `src/commands/`. Its `build.rs` injects `BUILD_TIME` and `BUILD_TARGET`; the supported spec version is the `APPCIPE_SPEC_VERSION` constant in `src/main.rs`.

## AppCipe Format Notes

See [examples/appcipe.yml](examples/appcipe.yml) for the fully-commented reference. Key semantics:
- `image:` accepts a short form (a bare tar path string) or a full form (`source`/`file`/`format`/`platform`) — both map to `appcipe_spec::ImageSourceOrPath`. Only `source: tar` is supported (dockerfile/image are rejected at validation).
- Persistence is opt-in per service via `persist_path`; data lands in `{data_dir or system default}/data/{service}/`. `old_names` drives automatic data-dir migration.
- `crash: fail_fast` (the only policy; legacy field name `crash_policy` accepted via serde alias): any service exiting non-zero terminates the whole app with that exit code.
- `interface_mode`: terminal | gui | both | none — at most one terminal/both service per app; host ports must be unique app-wide.

Validation rules live in `crates/appcipe-spec/src/validate.rs`; it collects **all** errors in one report (deterministic order).
