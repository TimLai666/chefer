use crate::types::*;
use std::path::Path;

pub fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<AppCipe> {
    let path = path.as_ref();
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let s = std::fs::read_to_string(path)?;
    from_str_with_base(&s, base)
}

pub fn from_str(yaml: &str) -> anyhow::Result<AppCipe> {
    let cwd = std::env::current_dir()?;
    from_str_with_base(yaml, &cwd)
}

pub fn from_str_with_base<P: AsRef<Path>>(yaml: &str, base: P) -> anyhow::Result<AppCipe> {
    let mut app: AppCipe = serde_yaml::from_str(yaml)?;
    normalize_paths_in_place(&mut app, base.as_ref())?;
    app.validate()
        .map_err(|e| anyhow::anyhow!("Validation error: {e}"))?;
    Ok(app)
}

fn to_abs(base: &Path, p: &str) -> String {
    let pb = Path::new(p);
    if pb.is_absolute() {
        p.to_string()
    } else {
        base.join(pb).to_string_lossy().to_string()
    }
}

fn normalize_paths_in_place(app: &mut AppCipe, base: &Path) -> anyhow::Result<()> {
    // data_dir（Host 路徑）
    if let Some(dir) = &app.data_dir {
        app.data_dir = Some(to_abs(base, dir));
    }

    for (_name, svc) in app.services.iter_mut() {
        // image.file（Host 路徑；僅 source=tar 或 TarPath）
        match &mut svc.image {
            ImageSourceOrPath::TarPath(p) => {
                *p = to_abs(base, p);
            }
            ImageSourceOrPath::Full { source, file, .. } => {
                if matches!(source, ImageSourceType::Tar) {
                    *file = to_abs(base, file);
                }
            }
        }

        // mounts：只轉左半邊（Host 路徑）。用 rsplitn(2, ':') 以相容 Windows 的 "C:\"
        // 形如 "host_path:container_path"
        for m in &mut svc.mounts {
            if let Some((host, guest)) = split_mount(m) {
                let new_host = to_abs(base, host);
                *m = format!("{new_host}:{guest}");
            }
        }

        // 注意：persist_path 是容器內路徑，不轉！
    }
    Ok(())
}

fn split_mount(s: &str) -> Option<(&str, &str)> {
    // 從右往左找一次 ':'，避免 Windows drive "C:\"
    let mut it = s.rsplitn(2, ':');
    let right = it.next()?;
    let left = it.next()?;
    Some((left, right))
}
