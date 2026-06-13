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
mod image;
mod layers;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use appcipe_spec::{AppCipe, Service};
use chefer_bundle::{
    AppMeta, CrashPolicy, LayerRef, MANIFEST_FORMAT_VERSION, Manifest, MountSpec, PortSpec,
    ServiceEntry, kit, layout,
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
            fs::remove_dir_all(&bundle_dir)
                .with_context(|| format!("清除既有 bundle 目錄失敗：{}", bundle_dir.display()))?;
        } else if fs::read_dir(&bundle_dir)?.next().is_some() {
            bail!(
                "輸出目錄已存在且非空：{}；請啟用 clean 選項或先手動刪除，以免殘留舊的層檔",
                bundle_dir.display()
            );
        }
    }
    fs::create_dir_all(&bundle_dir)?;

    // 依 service 名稱排序（輸出確定性，DESIGN §3）。
    let mut names: Vec<&String> = app.services.keys().collect();
    names.sort();

    // 先驗證所有 mounts 的 host 路徑存在（fail fast：避免解完大半個 image 才報錯）。
    for name in &names {
        validate_mounts(name, &app.services[*name])?;
    }

    let mut services = Vec::with_capacity(names.len());
    for name in &names {
        let svc = &app.services[*name];
        let entry = pack_service(&bundle_dir, name, svc, opts)
            .with_context(|| format!("打包 service `{name}` 失敗"))?;
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
            generated_at_utc: now_utc_rfc3339(),
        },
        services,
    };
    manifest.save(&layout::manifest_path(&bundle_dir))?;

    if opts.write_original_yml {
        let yml = serde_yaml::to_string(app).context("序列化 appcipe.yml 失敗")?;
        fs::write(layout::appcipe_out_path(&bundle_dir), yml)?;
    }

    copy_agents(&bundle_dir, &manifest, opts)?;
    copy_macos_appliances(&bundle_dir, opts)?;

    Ok(PackResult {
        bundle_dir,
        manifest,
    })
}

/// 驗證單一 service 的 mounts：語法可解析且 host 路徑存在。
fn validate_mounts(name: &str, svc: &Service) -> Result<()> {
    for m in &svc.mounts {
        let spec = MountSpec::parse(m)
            .with_context(|| format!("service `{name}` 的 mounts 設定錯誤：{m}"))?;
        if !Path::new(&spec.host).exists() {
            bail!(
                "service `{name}` 的掛載 host 路徑不存在：{}；請先建立該路徑，或修正 appcipe.yml 的 mounts",
                spec.host
            );
        }
    }
    Ok(())
}

/// 打包單一 service：解 image tar → 解析格式 → 逐層重壓 → 組 ServiceEntry。
fn pack_service(
    bundle_dir: &Path,
    name: &str,
    svc: &Service,
    opts: &PackOptions,
) -> Result<ServiceEntry> {
    let platform = convert::platform_of(svc);
    let tar_path_str = convert::image_tar_path(name, svc)?;
    let tar_path = Path::new(tar_path_str);
    if !tar_path.is_file() {
        bail!(
            "image tar 不存在：{}；請確認 appcipe.yml 的 image 路徑，或先以 `docker save -o <file> <image>` 匯出",
            tar_path.display()
        );
    }

    // 1) 安全解開 image tar 到暫存目錄（離開作用域自動清除）。
    let tmp = tempfile::Builder::new()
        .prefix("chefer-pack-")
        .tempdir()
        .context("建立暫存目錄失敗")?;
    archive::extract_tar_to_dir(tar_path, tmp.path())
        .with_context(|| format!("解開 image tar 失敗：{}", tar_path.display()))?;

    // 2) 偵測格式並依 platform 解析出 config 與層 blob。
    let resolved = image::resolve_image(tmp.path(), &platform)
        .with_context(|| format!("解析 image archive 失敗：{}", tar_path.display()))?;

    let diff_ids: &[String] = resolved
        .config
        .rootfs
        .as_ref()
        .map(|r| r.diff_ids.as_slice())
        .unwrap_or(&[]);
    if diff_ids.len() != resolved.layers.len() {
        bail!(
            "image config 的 rootfs.diff_ids 數量（{}）與層數（{}）不一致；image archive 可能已損毀，請重新匯出",
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
            .with_context(|| format!("處理第 {idx} 層失敗"))?;
        let expected = &diff_ids[idx];
        if &repacked.diff_id != expected {
            bail!(
                "第 {idx} 層 diff_id 不符：計算為 {}，config 宣告為 {expected}；image archive 可能已損毀，請重新匯出",
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
        .map(|p| PortSpec::parse(p).with_context(|| format!("ports 設定錯誤：{p}")))
        .collect::<Result<Vec<_>>>()?;
    let mounts = svc
        .mounts
        .iter()
        .map(|m| MountSpec::parse(m).with_context(|| format!("mounts 設定錯誤：{m}")))
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
    })
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
                    .with_context(|| format!("複製 guest-agent 失敗：{}", src.display()))?;
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
                    "警告：{help}\n（將不內嵌 guest-agent；Linux 目標可忽略，Windows/macOS 目標的單檔將無法執行）"
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
                    format!("複製 macOS appliance kernel 失敗：{}", kernel.display())
                })?;
                fs::copy(&initramfs, &initramfs_dst).with_context(|| {
                    format!(
                        "複製 macOS appliance initramfs 失敗：{}",
                        initramfs.display()
                    )
                })?;
            }
            None => {
                eprintln!(
                    "警告：找不到 macOS micro-VM appliance（需要 {} 與 {}）。\
                     已搜尋 kit 目錄：{}。\
                     將略過內嵌 vm/；此產物仍可組裝，但在 macOS 上會回報後端不可用。",
                    layout::kernel_name(arch),
                    layout::initramfs_name(arch),
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
        "（無可搜尋目錄）".to_string()
    } else {
        kit_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("、")
    }
}

/// 目前 UTC 時間的 RFC3339 字串（秒以下截斷，輸出穩定）。
fn now_utc_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    let now = now.replace_nanosecond(0).unwrap_or(now);
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
