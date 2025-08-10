// crates/chefer-pack/src/bundle.rs
use anyhow::{Result};
use fs_err as fs;
use std::path::{PathBuf};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use serde::Serialize;
use appcipe_spec::{AppCipe, Cmd};

pub struct Layout {
    pub bundle_dir: PathBuf,
    pub services_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub persist_map_path: PathBuf,
    pub appcipe_out_path: PathBuf,
}

pub fn prepare_layout(app: &AppCipe, opts: &crate::PackOptions) -> Result<Layout> {
    let bundle_dir = opts.out_dir.join(&app.name);
    if bundle_dir.exists() && opts.clean { fs::remove_dir_all(&bundle_dir)?; }
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
struct PersistEntry { service: String, container_path: String, host_rel: String }

#[derive(Serialize)]
struct ServiceManifest {
    name: String,
    rootfs_rel: String,
    persist_path: Option<String>,
    interface_mode: String,
    ports: Vec<String>,
    mounts: Vec<String>,
    cmd: Option<serde_json::Value>,   // 直接塞字串或陣列
    env: Vec<(String, String)>,
    workdir: Option<String>,
}

#[derive(Serialize)]
struct Manifest {
    app_name: String,
    spec_version: String,
    generated_at_utc: String,
    services: Vec<ServiceManifest>,
}

pub fn write_metadata(layout: &Layout, app: &AppCipe, opts: &crate::PackOptions) -> Result<()> {
    // manifest.json
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default();
    let mut services = vec![];
    for (name, svc) in &app.services {
        let cmd = match &svc.cmd {
            Some(Cmd::String(s)) => Some(serde_json::Value::String(s.clone())),
            Some(Cmd::Array(v))  => Some(serde_json::json!(v)),
            None => None,
        };
        services.push(ServiceManifest{
            name: name.clone(),
            rootfs_rel: format!("services/{name}/rootfs"),
            persist_path: svc.persist_path.clone(),
            interface_mode: format!("{:?}", svc.interface_mode).to_lowercase(),
            ports: svc.ports.clone(),
            mounts: svc.mounts.clone(),
            cmd,
            env: svc.env.iter().map(|(k,v)|(k.clone(), v.clone())).collect(),
            workdir: svc.workdir.clone(),
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
