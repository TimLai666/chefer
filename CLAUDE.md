# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chefer packages multiple Docker/OCI images (described by an "AppCipe" recipe, `appcipe.yml` — a Docker Compose-like format) into a single standalone executable that runs without Docker or any container engine. The end-user just downloads and runs one file; services run inside container isolation (Linux rootless namespaces, or a Chefer-dedicated WSL2 distro on Windows; macOS execution uses a planned Virtualization.framework micro-VM path whose Linux appliance is validated with QEMU first).

The architecture contract is [docs/DESIGN.md](docs/DESIGN.md) — the single source of truth for cross-crate formats, APIs and behavior. Change it first before changing anything cross-crate. Comments and docs are largely in Traditional Chinese.

## Commands

```powershell
cargo build                          # build the whole workspace
cargo build -p chefer-cli            # build one crate
cargo run -p chefer-cli -- check examples/appcipe.yml   # validate a recipe
cargo run -p chefer-cli -- build examples/appcipe.yml --dry-run
cargo test                           # run tests (cargo test -p <crate> for one crate)
cargo build -p guest-agent --target x86_64-unknown-linux-musl --release  # static agent (rust-lld preset in .cargo/config.toml)
CHEFER_LINUX_REF=v6.6.32 bash scripts/build-appliance.sh --arch x86_64 --out dist/appliance
CHEFER_LINUX_REF=v6.6.32 bash scripts/qemu-e2e.sh  # Linux only: validates appliance + guest-agent in QEMU
cargo fmt
cargo clippy
```

Testing: `cargo test` needs neither Docker nor WSL — integration tests (`crates/chefer-cli/tests/cli_e2e.rs`, `crates/chefer-pack/tests/pack_tests.rs`) synthesize minimal docker-archive/OCI image tars in-test and run init/check/build/inspect against the real CLI binary. A *full* manual E2E (real image → single-file exe → actually running it) additionally requires Docker (`docker save` an image) and, on a Windows host, WSL2. The macOS appliance path is validated on Linux with `scripts/qemu-e2e.sh`; the actual VZ boot path cannot be exercised on GitHub-hosted macOS runners and must be verified on a physical Mac.

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) builds and tests on Linux/Windows/macOS and verifies the musl guest-agent links statically. Linux E2E ([.github/workflows/e2e-linux.yml](.github/workflows/e2e-linux.yml)) covers both native namespaces and the QEMU appliance path. Releases ([.github/workflows/release.yml](.github/workflows/release.yml)) build chefer-cli + chefer-runtime for 6 targets, guest-agent for both musl arches, and the Linux appliance for both guest arches, then attach one complete kit per host platform (CLI + all runtimes + both agents + appliance) to the GitHub Release.

## Architecture

Cargo workspace under `crates/`. The data flow is:

**Build time:** `appcipe.yml` → **appcipe-spec** (Serde types, parsing, validation) → **appcipe-normalize** (host path absolutization, legacy-field migration; `appcipe_normalize::load` is the one official entry point — CLI and pack always go through it) → **chefer-pack** (parses each service's image tar — docker-archive or OCI layout, multi-arch resolved by `platform` — and stores the **original layers as zstd-compressed tars** plus `manifest.json` in a bundle dir; the bundle never contains an extracted rootfs, since a Windows host can't faithfully hold a Linux rootfs) → **chefer-assembler** (copies a prebuilt `chefer-runtime`, streams `zstd(tar(bundle/))` after it, writes an 80-byte footer → the final single-file executable).

**Run time:** the single file is **chefer-runtime** (reads its own footer; extracts the bundle into a **content-hashed cache dir keyed on the payload sha256** — `crates/chefer-runtime/src/extract.rs`; the first run stream-verifies sha256 while extracting, subsequent runs reuse the cache and skip re-extraction entirely (`--no-cache`/`--keep-tmp` force fresh temp extraction); resolves the data dir + `old_names` migration, starts host≠guest port proxies) → **vmm-backend** (picks the first available `ExecBackend`: Linux `namespaces` calls guest-agent in-process; Windows `wsl2` imports a minimal Chefer-dedicated WSL distro on first run and executes the bundle-embedded musl guest-agent inside it; macOS `vz` checks for a bundle-embedded Linux appliance and remains unavailable until the Virtualization.framework helper is validated on real hardware) → **guest-agent** (assembles the rootfs from layers applying OCI whiteouts, with caching + parallel layer decompression; **on root backends uses overlayfs** — per-layer content-addressed read-only lowerdirs + per-run writable upper, OCI whiteouts converted to overlay whiteouts, no merge-copy; rootless falls back to the merged path; binds persist dirs and mounts; starts services in `depends_on` topo order — gating dependents on a service's `healthcheck` until healthy; fail-fast supervision).

**chefer-bundle** is the shared protocol crate and sole definition of `manifest.json` serde types, the footer format, bundle layout helpers, `PortSpec`/`MountSpec` parsing, `topo_sort`, and **kit discovery** — other crates must import it, never hand-roll JSON/footer bytes.

**Kit:** `chefer build` needs prebuilt binaries — `chefer-runtime-<target-triple>[.exe]` per output target, `guest-agent-<arch>` (musl static; embedded into bundles for Windows/macOS targets), `pasta-<arch>` (musl static; optional — `network: bridge` outbound NAT; bundled into `agents/` when present, else `bridge` degrades to `internal` at runtime), and for macOS targets `chefer-vmlinuz-<arch>` / `chefer-initramfs-<arch>` appliance files. Search order: `--kit-dir` > `CHEFER_KIT_DIR` > `<exe dir>/kit/` > `<exe dir>`.

**chefer-cli** is the user-facing CLI (`init`, `check`, `build`, `run`, `inspect`, `version`, `upgrade`, `selfrm`), one file per command under `src/commands/`. `upgrade` replaces both the binary and the whole `kit/` together (no drift); `build` records its own `CARGO_PKG_VERSION` into `manifest.app.builder_version` (shown by `inspect` as "Packed by chefer" and in the runtime startup log); `selfrm` self-deletes the binary + kit and cleans the installer's PATH entry. Its `build.rs` injects `BUILD_TIME` and `BUILD_TARGET`; the supported spec version is the `APPCIPE_SPEC_VERSION` constant in `src/main.rs`.

## Versioning (read before bumping any version)

There are **several independent version numbers**; don't conflate them, and **never hardcode/assume the release number** — it comes from `Cargo.toml` (bumped at release) and the git tag, so docs say "latest release", not a number.

- **Chefer release version** — the `version` in the user-facing crates' `Cargo.toml` (`chefer-cli`, `chefer-runtime`, `chefer-pack`, `chefer-assembler`, `chefer-bundle`, `vmm-backend`, `guest-agent`). This is `CARGO_PKG_VERSION`; it drives `chefer version`, `chefer upgrade` (compared against the GitHub Release tag), and is written into `manifest.app.builder_version` (shown by `inspect` as "Packed by chefer" + in the runtime startup log). Bump it to match the release git tag (e.g. tag `v0.2.0` ⇒ these crates `0.2.0`).
- **`appcipe-spec` / `appcipe-normalize` crate versions track the *appcipe.yml spec version*, NOT the release** (see the comments in their `Cargo.toml`): the first two digits follow `APPCIPE_SPEC_VERSION`. They only move when the appcipe.yml format changes — so they can legitimately differ from the release version.
- **`APPCIPE_SPEC_VERSION`** (`crates/chefer-cli/src/main.rs`, currently `"0.1"`) — the appcipe.yml schema version the CLI accepts/validates. Independent of the release.
- **`MANIFEST_FORMAT_VERSION`** (`crates/chefer-bundle/src/manifest.rs`, currently `1`) — bundle `manifest.json` format; the runtime rejects a mismatch. Independent format version.
- **`FOOTER_VERSION`** + `MAGIC` (`crates/chefer-bundle/src/footer.rs`, currently `1` / `b"CHEFER\0\0"`) — single-file footer format; the runtime rejects a mismatch. Independent format version.

Rule of thumb: the **release version** moves every release; the **spec/manifest/footer** versions are *format/protocol* versions that move **only on an actual incompatible format change** — and when one does, update its reader/validator (which already checks it) **and** `docs/DESIGN.md`.

## AppCipe Format Notes

**Authoring an `appcipe.yml`?** Read [skills/write-appcipe/SKILL.md](skills/write-appcipe/SKILL.md) — the full field reference, validation rules, and real-world gotchas (image `format` is hyphenated, official chown/gosu images (redis/postgres) run as-is on the root backends — WSL2 / macOS VM / native-Linux-as-root run services as real root with no user namespace — and on the native-Linux rootless path too when the host has `/etc/subuid` + `newuidmap` (rootless-Podman-style delegation; falls back to a single-uid map without them), the app-level `network:` field (default `bridge` = per-app netns isolation + outbound NAT; `internal` = no outbound; `shared` = old non-isolated behavior), `app_version` is display-only and not readable by the container). A runnable app+db example lives in [examples/demo/](examples/demo/).

See [examples/appcipe.yml](examples/appcipe.yml) for the fully-commented reference. Key semantics:
- `image:` accepts a short form or a full form (`source`/`file`/`format`/`platform`) — both map to `appcipe_spec::ImageSourceOrPath`. **`source: tar`** (local `docker save`/OCI archive) and **`source: image`** (registry pull at build time via `oci-client`, rustls, anonymous/public only) are supported; `source: dockerfile` is rejected at validation. The short form auto-detects: a pinned registry ref (`redis:7.2-alpine`, has `:tag`/`@digest`, not a path/`.tar`/existing file — see `appcipe-normalize::looks_like_image_ref`) → `source: image`; otherwise a tar path. Registry refs **must** pin a non-`latest` tag or `@sha256` digest (`appcipe-spec` `check_image_reference`). The registry pull lives in `chefer-pack::registry` and produces the same `ResolvedImage` as a local tar, so the repack/diff_id path is shared.
- Persistence is opt-in per service via `persist_path`; data lands in `{data_dir or system default}/data/{service}/`. `old_names` drives automatic data-dir migration.
- `crash: fail_fast` (the only policy; legacy field name `crash_policy` accepted via serde alias): any service exiting non-zero terminates the whole app with that exit code. Additionally, an **interface service** (`interface_mode` gui/terminal/both) exiting *even with code 0* tears the whole app down (closing the window/terminal = app done) — otherwise background services like a `db` would linger after the GUI closes; a `none` service exiting 0 is treated as a finished background/one-shot task and the rest keep running.
- `interface_mode`: terminal | gui | both | none — at most one terminal/both service per app; host ports must be unique app-wide.

Validation rules live in `crates/appcipe-spec/src/validate.rs`; it collects **all** errors in one report (deterministic order).
