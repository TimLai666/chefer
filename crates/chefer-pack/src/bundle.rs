use anyhow::{Result, bail};
use fs_err as fs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use serde::Serialize;
use appcipe_spec::{AppCipe, Cmd, ImageFormat, ImagePlatform, ImageSourceOrPath};

pub struct Layout {
    pub bundle_dir: PathBuf,
    pub services_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub persist_map_path: PathBuf,
    pub appcipe_out_path: PathBuf,
}

pub fn prepare_layout(app: &AppCipe, opts: &crate::PackOptions) -> Result<Layout> {
    let bundle_dir = opts.out_dir.join(&app.name);
    if bundle_dir.exists() && opts.clean {
        fs::remove_dir_all(&bundle_dir)?;
    }
    fs::create_dir_all(bundle_dir.join("services"))?;
    Ok(Layout {
        services_dir: bundle_dir.join("services"),
        manifest_path: bundle_dir.join("manifest.json"),
        persist_map_path: bundle_dir.join("persist-map.json"),
        appcipe_out_path: bundle_dir.join("appcipe.yml"),
        bundle_dir,
    })
}

#[derive(Serialize)]
struct PersistEntry {
    service: String,
    container_path: String,
    host_rel: String,
}

#[derive(Serialize)]
struct ServiceManifest {
    name: String,
    rootfs_rel: String,
    persist_path: Option<String>,
    interface_mode: String,
    ports: Vec<String>,
    mounts: Vec<String>,
    cmd: Option<serde_json::Value>,   // 字串或陣列
    env: Vec<(String, String)>,       // 排序過
    workdir: Option<String>,
    // 追加
    depends_on: Vec<String>,
    platform: Option<String>,         // "linux/amd64"...
    image_format: Option<String>,     // "docker-archive"/"oci-archive"/"auto"
}

#[derive(Serialize)]
struct Manifest {
    app_name: String,
    spec_version: String,
    generated_at_utc: String,
    services: Vec<ServiceManifest>,
}

pub fn write_metadata(layout: &Layout, app: &AppCipe, opts: &crate::PackOptions) -> Result<()> {
    // 先做 mounts 主機端存在性驗證
    for (name, svc) in &app.services {
        for m in &svc.mounts {
            if let Some((host, _guest)) = split_mount(m) {
                let hp = Path::new(host);
                if !hp.exists() {
                    bail!("service `{name}` mount host path not found: {host}");
                }
            } else {
                bail!("service `{name}` invalid mount syntax: {m}");
            }
        }
    }

    // manifest.json
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default();
    let mut services = vec![];

    for (name, svc) in &app.services {
        let cmd = match &svc.cmd {
            Some(Cmd::String(s)) => Some(serde_json::Value::String(s.clone())),
            Some(Cmd::Array(v))  => Some(serde_json::json!(v)),
            None => None,
        };

        // env 排序（穩定輸出）
        let mut env_sorted: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &svc.env {
            env_sorted.insert(k.clone(), v.clone());
        }
        let env_vec: Vec<(String, String)> =
            env_sorted.into_iter().collect();

        // 讀取 platform / image_format（若有）
        let (platform, image_format) = match &svc.image {
            ImageSourceOrPath::TarPath(_) => (None, Some("auto".to_string())),
            ImageSourceOrPath::Full { platform, format, .. } => {
                let pf = match platform {
                    ImagePlatform::LinuxAmd64   => "linux/amd64",
                    ImagePlatform::LinuxArm64   => "linux/arm64",
                    ImagePlatform::WindowsAmd64 => "windows/amd64",
                }.to_string();
                let fmt = match format {
                    ImageFormat::Auto          => "auto",
                    ImageFormat::DockerArchive => "docker-archive",
                    ImageFormat::OciArchive    => "oci-archive",
                }.to_string();
                (Some(pf), Some(fmt))
            }
        };

        services.push(ServiceManifest {
            name: name.clone(),
            rootfs_rel: format!("services/{name}/rootfs"),
            persist_path: svc.persist_path.clone(),
            interface_mode: format!("{:?}", svc.interface_mode).to_lowercase(),
            ports: svc.ports.clone(),
            mounts: svc.mounts.clone(),
            cmd,
            env: env_vec,
            workdir: svc.workdir.clone(),
            depends_on: svc.depends_on.clone(),
            platform,
            image_format,
        });
    }

    let mani = Manifest {
        app_name: app.name.clone(),
        spec_version: app.version.clone(),
        generated_at_utc: now,
        services,
    };
    fs::write(&layout.manifest_path, serde_json::to_vec_pretty(&mani)?)?;

    // persist-map.json（host 相對路徑規則：data/<service>）
    let mut persist = vec![];
    for (name, svc) in &app.services {
        if let Some(p) = &svc.persist_path {
            // persist_path 是容器內路徑，必須以 "/" 開頭
            if !p.starts_with('/') {
                bail!("service `{name}` persist_path must be absolute container path, got `{p}`");
            }
            persist.push(PersistEntry {
                service: name.clone(),
                container_path: p.clone(),
                host_rel: format!("data/{name}"),
            });
        }
    }
    fs::write(&layout.persist_map_path, serde_json::to_vec_pretty(&persist)?)?;

    if opts.write_original_yml {
        let yml = serde_yaml::to_string(app)?;
        fs::write(&layout.appcipe_out_path, yml)?;
    }
    Ok(())
}

impl Layout {
    pub fn svc_rootfs_dir(&self, name: &str) -> PathBuf {
        self.services_dir.join(name).join("rootfs")
    }
}

/// 解析 "<host>:<container>"，從右往左切割，避免 Windows "C:\"
pub(crate) fn split_mount(s: &str) -> Option<(&str, &str)> {
    let mut it = s.rsplitn(2, ':');
    let right = it.next()?;
    let left  = it.next()?;
    Some((left, right))
}
