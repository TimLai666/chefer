# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chefer packages multiple Docker/OCI images (described by an "AppCipe" recipe, `appcipe.yml` — a Docker Compose-like format) into a single standalone executable that runs without Docker or any container engine. The end-user just downloads and runs one file; services execute inside a microVM. The project is an early-stage MVP — several crates are still stubs.

Comments and docs are largely in Traditional Chinese.

## Commands

```powershell
cargo build                          # build the whole workspace
cargo build -p chefer-cli            # build one crate
cargo run -p chefer-cli -- check examples/appcipe.yml   # validate a recipe
cargo run -p chefer-cli -- build examples/appcipe.yml --dry-run
cargo test                           # run tests (cargo test -p <crate> for one crate)
cargo fmt
cargo clippy
```

Release binaries are built by GitHub Actions on release publish ([.github/workflows/main.yml](.github/workflows/main.yml)) for chefer-cli, chefer-runtime, and guest-agent across Linux/Windows/macOS targets.

## Architecture

Cargo workspace under `crates/`. The data flow is:

**Build time:** `appcipe.yml` → **appcipe-spec** (Serde types, parsing, defaults, validation; entry point `appcipe_spec::from_file`) → **chefer-pack** (extracts each service's rootfs from image tars, writes manifest/persist-map into a bundle dir; public API in `api.rs`: `pack_all(app, &PackOptions) -> PackResult`) → **chefer-assembler** (stub; will produce the final single-file executable).

**Run time:** the single file is **chefer-runtime** (reads a trailing footer in its own exe — offset/length/sha256 of the appended bundle — extracts it to a temp dir, then runs it) → **vmm-backend** (stub; KVM/HVF/WHPX abstraction for the microVM) → **guest-agent** (PID 1 inside the VM; starts services per the appcipe and monitors them).

**chefer-cli** is the user-facing CLI (`check`, `build`, `version`, `upgrade`). It calls `appcipe_spec` for validation and `chefer_pack` for building. Its `build.rs` injects `BUILD_TIME` and `BUILD_TARGET` env vars used by `version`. The supported spec version is the `APPCIPE_SPEC_VERSION` constant in `chefer-cli/src/main.rs`.

**appcipe-normalize** (stub) will migrate old/legacy AppCipe fields to current ones and apply path rules.

## AppCipe Format Notes

See [examples/appcipe.yml](examples/appcipe.yml) for the fully-commented reference. Key semantics:
- `image:` accepts a short form (a bare tar path string) or a full form (`source`/`file`/`format`/`platform`) — both map to `appcipe_spec::ImageSourceOrPath`. MVP supports `source: tar` only.
- Persistence is opt-in per service via `persist_path`; data lands in `{data_dir or system default}/{name}/data/{service}/`. `old_names` drives automatic data-dir migration.
- `crash: fail_fast` (MVP's only policy): any service exiting non-zero exits the whole app.
- `interface_mode`: terminal | gui | both | none.

Validation rules live in `crates/appcipe-spec/src/validate.rs` (includes name/service-name validity and path-safety checks).
