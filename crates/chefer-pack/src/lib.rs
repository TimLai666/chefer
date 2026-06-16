//! chefer-pack — 將 appcipe 描述的 Docker/OCI image tar 解析為 chefer bundle。
//!
//! 流程（規格見 docs/DESIGN.md §6 chefer-pack 節、§2 bundle 佈局、§3 manifest）：
//! 1. 將每個 service 的 image tar 安全解到暫存目錄，依內容偵測格式
//!    （OCI layout 或傳統 docker-archive），並依 service platform 挑選 image。
//! 2. 每層串流解壓（gzip / zstd / 未壓縮）→ 計算 diff_id（未壓縮 tar 的 sha256）
//!    → zstd 重壓寫入 `services/<svc>/layers/`，並與 config 的 rootfs.diff_ids 比對。
//! 3. 產出 manifest.json（chefer_bundle::Manifest）與（選）appcipe.yml 回寫。
//! 4. 從 kit 複製對應 guest 架構的 musl guest-agent 到 `agents/`。
//! 5. 建置 macOS 目標時，best-effort 從 kit 複製 micro-VM appliance 到 `vm/`。

mod archive;
mod convert;
mod dockerfile;
mod image;
mod layers;
mod registry;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use appcipe_spec::{AppCipe, Service};
use chefer_bundle::{
    AppMeta, CrashPolicy, LayerRef, MANIFEST_FORMAT_VERSION, Manifest, MountSpec, NetworkMode,
    PortSpec, ServiceEntry, kit, layout,
};
use fs_err as fs;

/// 打包選項。
#[derive(Clone, Debug)]
pub struct PackOptions {
    /// 輸出根目錄；bundle 會寫到 `<out_dir>/<app.name>/bundle/`。
    pub out_dir: PathBuf,
    /// 為 true 時先刪除既有 bundle 目錄再重建。
    pub clean: bool,
    /// 是否將原始 appcipe 設定回寫到 `bundle/appcipe.yml`。
    pub write_original_yml: bool,
    /// kit 搜尋目錄（會排在預設搜尋順序之前；用於尋找 guest-agent）。
    pub kit_dirs: Vec<PathBuf>,
    /// 這次 `chefer build` 要組裝的 target triples；用於判斷是否需內嵌 macOS appliance。
    pub target_triples: Vec<String>,
    /// 找不到 guest-agent 時是否視為錯誤（false 時僅印出警告）。
    pub require_agents: bool,
    /// 層重壓所用的 zstd 壓縮等級。
    pub zstd_level: i32,
    /// 打包此 app 的 chefer 版本（寫入 manifest 的 `app.builder_version`）。
    /// CLI 傳入自身的 `CARGO_PKG_VERSION`；其他呼叫端可留空字串。
    pub builder_version: String,
    /// 是否為 `chefer run` 的本機立即執行（打包後馬上在本機跑）。
    /// 只有此值為 true 且 build 不含 VM（darwin）目標時，缺 mount host 路徑才視為錯誤；
    /// 其餘（`chefer build` 的散布產物、或含 darwin 目標）降級為警告，交由執行期重新檢查。
    /// 見 `mount_check_is_lenient`。
    pub local_run: bool,
}

/// 打包結果。
#[derive(Clone, Debug)]
pub struct PackResult {
    /// bundle 根目錄（`<out_dir>/<app.name>/bundle`）。
    pub bundle_dir: PathBuf,
    /// 寫入 bundle 的 manifest（與 manifest.json 內容一致）。
    pub manifest: Manifest,
}

/// 將 `app` 描述的所有 service 打包為 bundle。
pub fn pack(app: &AppCipe, opts: &PackOptions) -> Result<PackResult> {
    let bundle_dir = opts.out_dir.join(&app.name).join("bundle");
    if bundle_dir.exists() {
        if opts.clean {
            fs::remove_dir_all(&bundle_dir).with_context(|| {
                format!(
                    "failed to remove existing bundle directory: {}",
                    bundle_dir.display()
                )
            })?;
        } else if fs::read_dir(&bundle_dir)?.next().is_some() {
            bail!(
                "output directory already exists and is not empty: {}; enable the clean option or delete it manually first to avoid leftover layer files",
                bundle_dir.display()
            );
        }
    }
    fs::create_dir_all(&bundle_dir)?;

    // 依 service 名稱排序（輸出確定性，DESIGN §3）。
    let mut names: Vec<&String> = app.services.keys().collect();
    names.sort();

    // 先驗證所有 mounts 的 host 路徑存在（fail fast：避免解完大半個 image 才報錯）。
    // 此檢查假設「打包機 == 執行機」，只在 `chefer run`（本機立即執行）且非 VM 後端時成立；
    // `chefer build` 的散布產物與 VM 後端的 host 都是別台機器／guest VM，缺路徑只降級為警告，
    // 交由 guest-agent 執行期在真正的 host 重新檢查（見 DESIGN.md guest-agent 服務啟動節）。
    let lenient_mounts = mount_check_is_lenient(opts);
    for name in &names {
        validate_mounts(name, &app.services[*name], lenient_mounts)?;
    }

    let mut services = Vec::with_capacity(names.len());
    for name in &names {
        let svc = &app.services[*name];
        let entry = pack_service(&bundle_dir, name, svc, opts)
            .with_context(|| format!("failed to pack service `{name}`"))?;
        services.push(entry);
    }

    let manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        app: AppMeta {
            name: app.name.clone(),
            app_version: app.app_version.clone(),
            spec_version: app.version.clone(),
            old_names: app.old_names.clone(),
            data_dir_override: app.data_dir.clone(),
            crash: CrashPolicy::FailFast,
            network: map_network(app.network),
            console: map_console(app.console),
            generated_at_utc: now_utc_rfc3339(),
            builder_version: opts.builder_version.clone(),
        },
        services,
    };
    manifest.save(&layout::manifest_path(&bundle_dir))?;

    if opts.write_original_yml {
        let yml = serde_yaml::to_string(app).context("failed to serialize appcipe.yml")?;
        fs::write(layout::appcipe_out_path(&bundle_dir), yml)?;
    }

    copy_agents(&bundle_dir, &manifest, opts)?;
    copy_macos_appliances(&bundle_dir, opts)?;

    Ok(PackResult {
        bundle_dir,
        manifest,
    })
}

/// 驗證單一 service 的 mounts：語法可解析；host 路徑存在性視 `lenient` 決定嚴格度。
///
/// `lenient == true`（打包機 ≠ 執行機：`chefer build` 散布產物或 VM 後端）時，缺路徑只印
/// 警告而非報錯——交由 guest-agent 於執行期在真正的 host 重新檢查。見 `mount_check_is_lenient`。
fn validate_mounts(name: &str, svc: &Service, lenient: bool) -> Result<()> {
    for m in &svc.mounts {
        let spec = MountSpec::parse(m)
            .with_context(|| format!("invalid mounts setting for service `{name}`: {m}"))?;
        if !Path::new(&spec.host).exists() {
            if lenient {
                eprintln!(
                    "warning: mount host path does not exist on the build machine for service `{name}`: {}. \
                     The single-file executable runs on a different machine (or inside a VM), so this path is verified at runtime, not now. \
                     Make sure it will exist on the machine that runs the app.",
                    spec.host
                );
            } else {
                bail!(
                    "mount host path does not exist for service `{name}`: {}; create the path first, or fix the mounts in appcipe.yml",
                    spec.host
                );
            }
        }
    }
    Ok(())
}

/// 打包期 mount host 路徑檢查是否放寬為警告。
///
/// 嚴格（回 false，缺路徑報錯）只在「打包機 == 執行機」時成立：即 `chefer run`
/// （`local_run == true`，本機建置後立即執行）且 build **不含** VM（darwin）目標。
/// 其餘一律放寬（回 true）——`chefer build` 的產物是要散布到別台機器執行，在打包機檢查
/// 路徑等於檢查錯的機器；VM 後端（macOS vz／appliance）的 host 是 guest VM，同樣 host≠build-host。
fn mount_check_is_lenient(opts: &PackOptions) -> bool {
    let has_vm_target = opts
        .target_triples
        .iter()
        .any(|t| macos_target_arch(t).is_some());
    !opts.local_run || has_vm_target
}

/// 打包單一 service：解 image tar → 解析格式 → 逐層重壓 → 組 ServiceEntry。
fn pack_service(
    bundle_dir: &Path,
    name: &str,
    svc: &Service,
    opts: &PackOptions,
) -> Result<ServiceEntry> {
    let platform = convert::platform_of(svc);

    // 暫存目錄：tar 解壓內容或 registry 拉下的 layer blob 都放這；持有到 repack 完成。
    let tmp = tempfile::Builder::new()
        .prefix("chefer-pack-")
        .tempdir()
        .context("failed to create temp directory")?;

    // 取得 config + 各層 blob 路徑——本機 tar 與 registry 兩條來源都收斂成同一個 ResolvedImage。
    let resolved = match convert::image_src(name, svc)? {
        convert::ImageSrc::Tar(tar_path_str) => {
            let tar_path = Path::new(tar_path_str);
            if !tar_path.is_file() {
                bail!(
                    "image tar not found: {}; check the image path in appcipe.yml, or export it first with `docker save -o <file> <image>` (or use source: image to pull directly from a registry)",
                    tar_path.display()
                );
            }
            archive::extract_tar_to_dir(tar_path, tmp.path())
                .with_context(|| format!("failed to extract image tar: {}", tar_path.display()))?;
            image::resolve_image(tmp.path(), &platform)
                .with_context(|| format!("failed to parse image archive: {}", tar_path.display()))?
        }
        convert::ImageSrc::Registry(reference) => {
            eprintln!("[chefer] pulling image from registry: {reference} ({platform})…");
            registry::pull_image(reference, &platform, tmp.path())
                .with_context(|| format!("failed to pull image `{reference}` from registry"))?
        }
        convert::ImageSrc::Dockerfile {
            dockerfile,
            context,
            build_args,
        } => {
            // 在打包機上以既有 builder 建置成 docker-archive tar，再併入與 tar 相同的解析路徑。
            let tar = dockerfile::build_image_tar(
                name,
                dockerfile,
                context,
                build_args,
                &platform,
                tmp.path(),
            )?;
            let extract = tmp.path().join("extract");
            fs::create_dir_all(&extract)?;
            archive::extract_tar_to_dir(&tar, &extract)
                .with_context(|| format!("failed to extract built image tar: {}", tar.display()))?;
            image::resolve_image(&extract, &platform)
                .context("failed to parse built image archive")?
        }
    };

    let diff_ids: &[String] = resolved
        .config
        .rootfs
        .as_ref()
        .map(|r| r.diff_ids.as_slice())
        .unwrap_or(&[]);
    if diff_ids.len() != resolved.layers.len() {
        bail!(
            "image config rootfs.diff_ids count ({}) does not match the number of layers ({}); the image archive may be corrupt, please re-export",
            diff_ids.len(),
            resolved.layers.len()
        );
    }

    // 3) 逐層串流重壓為 zstd，並驗證 diff_id。
    let layers_dir = layout::service_layers_dir(bundle_dir, name);
    fs::create_dir_all(&layers_dir)?;
    let mut layer_refs = Vec::with_capacity(resolved.layers.len());
    for (idx, blob) in resolved.layers.iter().enumerate() {
        let repacked = layers::repack_layer(blob, &layers_dir, idx, opts.zstd_level)
            .with_context(|| format!("failed to process layer {idx}"))?;
        let expected = &diff_ids[idx];
        if &repacked.diff_id != expected {
            bail!(
                "layer {idx} diff_id mismatch: computed {}, config declares {expected}; the image archive may be corrupt, please re-export",
                repacked.diff_id
            );
        }
        layer_refs.push(LayerRef {
            rel_path: format!("services/{name}/layers/{}", repacked.file_name),
            diff_id: repacked.diff_id,
            size: Some(repacked.size),
        });
    }

    // 4) appcipe Service → manifest ServiceEntry。
    let ports = svc
        .ports
        .iter()
        .map(|p| PortSpec::parse(p).with_context(|| format!("invalid ports setting: {p}")))
        .collect::<Result<Vec<_>>>()?;
    let mounts = svc
        .mounts
        .iter()
        .map(|m| MountSpec::parse(m).with_context(|| format!("invalid mounts setting: {m}")))
        .collect::<Result<Vec<_>>>()?;

    Ok(ServiceEntry {
        name: name.to_string(),
        platform,
        layers: layer_refs,
        image_config: convert::to_image_config(&resolved.config),
        cmd_override: convert::to_cmd_spec(svc.cmd.as_ref()),
        env: svc
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        workdir_override: svc.workdir.clone(),
        persist_path: svc.persist_path.clone(),
        ports,
        mounts,
        interface_mode: convert::to_interface_mode(&svc.interface_mode),
        depends_on: svc.depends_on.clone(),
        healthcheck: svc.healthcheck.as_ref().map(map_healthcheck),
    })
}

/// appcipe healthcheck → manifest healthcheck（duration 正規化為毫秒；test 正規化為 CmdSpec）。
/// 驗證已保證 test 非空、duration 可解析；此處 unwrap_or(default) 以防萬一。
fn map_healthcheck(hc: &appcipe_spec::HealthCheck) -> chefer_bundle::HealthCheck {
    use appcipe_spec::TestCmd;
    let test = match hc.test.to_cmd() {
        Some(TestCmd::Shell(s)) => chefer_bundle::CmdSpec::Shell(s),
        Some(TestCmd::Argv(v)) => chefer_bundle::CmdSpec::Argv(v),
        // 驗證期已擋空 test；保底給一個永遠失敗的探針而非 panic。
        None => chefer_bundle::CmdSpec::Argv(vec!["false".to_string()]),
    };
    let ms = |d: std::time::Duration| d.as_millis() as u64;
    chefer_bundle::HealthCheck {
        test,
        interval_ms: ms(hc
            .interval()
            .unwrap_or(appcipe_spec::HealthCheck::DEFAULT_INTERVAL)),
        timeout_ms: ms(hc
            .timeout()
            .unwrap_or(appcipe_spec::HealthCheck::DEFAULT_TIMEOUT)),
        retries: hc.retries(),
        start_period_ms: ms(hc
            .start_period()
            .unwrap_or(appcipe_spec::HealthCheck::DEFAULT_START_PERIOD)),
    }
}

/// 對 services 用到的每個 linux guest 架構，從 kit 複製 musl guest-agent 到 agents/。
fn copy_agents(bundle_dir: &Path, manifest: &Manifest, opts: &PackOptions) -> Result<()> {
    let mut arches: BTreeSet<&'static str> = BTreeSet::new();
    for s in &manifest.services {
        if let Some(a) = layout::platform_to_arch(&s.platform) {
            arches.insert(a);
        }
    }
    if arches.is_empty() {
        return Ok(());
    }

    // 搜尋順序：呼叫端指定的 kit_dirs 優先，其後為預設順序（DESIGN §5）。
    let mut kit_dirs = opts.kit_dirs.clone();
    kit_dirs.extend(kit::default_kit_dirs());

    let agents_dir = layout::agents_dir(bundle_dir);
    for arch in arches {
        let agent_name = layout::guest_agent_name(arch);
        match kit::find_guest_agent(&kit_dirs, arch) {
            Some(src) => {
                fs::create_dir_all(&agents_dir)?;
                let dst = agents_dir.join(&agent_name);
                fs::copy(&src, &dst)
                    .with_context(|| format!("failed to copy guest-agent: {}", src.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = std::fs::metadata(&dst)?.permissions();
                    perm.set_mode(0o755);
                    std::fs::set_permissions(&dst, perm)?;
                }
            }
            None => {
                let help = kit::not_found_help(&kit_dirs, &agent_name);
                if opts.require_agents {
                    bail!("{help}");
                }
                eprintln!(
                    "warning: {help}\n(guest-agent will not be embedded; Linux targets can ignore this, but the single-file executable for Windows/macOS targets will not run)"
                );
            }
        }

        // pasta（bridge 出網用）：可選。缺少時僅提示——bridge 模式會在執行期退化為
        // internal（只有 lo、無對外網路），不影響 shared/internal 的 app。
        let pasta_name = layout::pasta_name(arch);
        match kit::find_pasta(&kit_dirs, arch) {
            Some(src) => {
                fs::create_dir_all(&agents_dir)?;
                let dst = agents_dir.join(&pasta_name);
                fs::copy(&src, &dst)
                    .with_context(|| format!("failed to copy pasta: {}", src.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = std::fs::metadata(&dst)?.permissions();
                    perm.set_mode(0o755);
                    std::fs::set_permissions(&dst, perm)?;
                }
            }
            None => {
                eprintln!(
                    "note: {pasta_name} not found in the kit; an app packed with network: bridge will \
                     degrade to internal at runtime (only lo, no outbound network). To enable bridge outbound, add pasta-{arch} to the kit."
                );
            }
        }
    }
    Ok(())
}

/// 建置 macOS target 時，將對應架構的 Linux micro-VM appliance 複製到 `vm/`。
///
/// appliance 尚未進入 kit 前依 DESIGN §2 採 best-effort：缺少時警告但不阻斷建置；
/// 產物仍可 assemble，只是在 macOS 執行時 availability/run 會給出明確錯誤。
fn copy_macos_appliances(bundle_dir: &Path, opts: &PackOptions) -> Result<()> {
    let mut arches: BTreeSet<&'static str> = BTreeSet::new();
    for target in &opts.target_triples {
        if let Some(arch) = macos_target_arch(target) {
            arches.insert(arch);
        }
    }
    if arches.is_empty() {
        return Ok(());
    }

    let mut kit_dirs = opts.kit_dirs.clone();
    kit_dirs.extend(kit::default_kit_dirs());

    let vm_dir = layout::vm_dir(bundle_dir);
    for arch in arches {
        match kit::find_appliance(&kit_dirs, arch) {
            Some((kernel, initramfs)) => {
                fs::create_dir_all(&vm_dir)?;
                let kernel_dst = vm_dir.join(layout::kernel_name(arch));
                let initramfs_dst = vm_dir.join(layout::initramfs_name(arch));
                fs::copy(&kernel, &kernel_dst).with_context(|| {
                    format!(
                        "failed to copy macOS appliance kernel: {}",
                        kernel.display()
                    )
                })?;
                fs::copy(&initramfs, &initramfs_dst).with_context(|| {
                    format!(
                        "failed to copy macOS appliance initramfs: {}",
                        initramfs.display()
                    )
                })?;
            }
            None => {
                eprintln!(
                    "warning: macOS micro-VM appliance not found (need {} and {}). \
                     Searched kit directories: {}. \
                     Skipping embedding vm/; this artifact can still be assembled, but will report the backend as unavailable on macOS.",
                    layout::kernel_name(arch),
                    layout::initramfs_name(arch),
                    format_kit_dirs(&kit_dirs),
                );
            }
        }

        // vz 開機 helper（host macho，已於 release 以 virtualization entitlement 簽章）→ agents/。
        // best-effort：缺少時警告但不阻斷（執行期 vz 後端會回報 helper 缺失，或可用 CHEFER_VZ_HELPER）。
        let helper_name = layout::vz_helper_name(arch);
        match kit::find_vz_helper(&kit_dirs, arch) {
            Some(src) => {
                let agents_dir = layout::agents_dir(bundle_dir);
                fs::create_dir_all(&agents_dir)?;
                let dst = agents_dir.join(&helper_name);
                fs::copy(&src, &dst).with_context(|| {
                    format!("failed to copy macOS vz helper: {}", src.display())
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = std::fs::metadata(&dst)?.permissions();
                    perm.set_mode(0o755);
                    std::fs::set_permissions(&dst, perm)?;
                }
            }
            None => {
                eprintln!(
                    "warning: macOS vz helper not found (need {}). Searched kit directories: {}. \
                     Skipping embedding it; the macOS vz backend will be unavailable unless CHEFER_VZ_HELPER points to a locally built helper.",
                    helper_name,
                    format_kit_dirs(&kit_dirs),
                );
            }
        }
    }
    Ok(())
}

fn macos_target_arch(target: &str) -> Option<&'static str> {
    match target {
        "x86_64-apple-darwin" => Some("x86_64"),
        "aarch64-apple-darwin" => Some("aarch64"),
        _ => None,
    }
}

fn format_kit_dirs(kit_dirs: &[PathBuf]) -> String {
    if kit_dirs.is_empty() {
        "(no directories to search)".to_string()
    } else {
        kit_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 目前 UTC 時間的 RFC3339 字串（秒以下截斷，輸出穩定）。
/// 把 appcipe-spec 的網路模式映射到 bundle manifest 的網路模式。
/// 兩個 enum 故意各自獨立（spec 跟 appcipe.yml 格式走、manifest 跟協定走）。
fn map_console(c: appcipe_spec::ConsoleMode) -> chefer_bundle::ConsoleMode {
    use chefer_bundle::ConsoleMode as B;
    match c {
        appcipe_spec::ConsoleMode::Auto => B::Auto,
        appcipe_spec::ConsoleMode::Shown => B::Shown,
        appcipe_spec::ConsoleMode::Hidden => B::Hidden,
    }
}

fn map_network(n: appcipe_spec::NetworkMode) -> NetworkMode {
    match n {
        appcipe_spec::NetworkMode::Shared => NetworkMode::Shared,
        appcipe_spec::NetworkMode::Internal => NetworkMode::Internal,
        appcipe_spec::NetworkMode::Bridge => NetworkMode::Bridge,
    }
}

fn now_utc_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    let now = now.replace_nanosecond(0).unwrap_or(now);
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
