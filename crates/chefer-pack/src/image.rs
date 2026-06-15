//! image archive 的格式偵測與解析（docker-archive 與 OCI layout）。
//!
//! 偵測規則（看內容不看副檔名；docs/DESIGN.md §6）：
//! - 有 `index.json` + `blobs/` → OCI layout（oci-archive 與新版 docker save 都是）。
//! - 否則有 `manifest.json`（JSON 陣列）→ 傳統 docker-archive。
//! - 都沒有 → 不是有效的 image archive。

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use serde::Deserialize;

use crate::archive::sanitize_rel_path;

/// 解析完成的 image：config（寬鬆解析）+ 依套用順序的層 blob 路徑。
#[derive(Debug)]
pub(crate) struct ResolvedImage {
    pub config: ImageConfigJson,
    /// 各層 blob 在暫存目錄內的絕對路徑（可能為 tar / tar+gzip / tar+zstd）。
    pub layers: Vec<PathBuf>,
}

/// image config JSON（serde 寬鬆解析：未知欄位忽略、欄位可能為 null）。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ImageConfigJson {
    pub architecture: Option<String>,
    pub os: Option<String>,
    pub config: Option<RuntimeConfigJson>,
    pub rootfs: Option<RootFsJson>,
}

/// config 物件內的執行設定（注意大小寫：OCI/Docker 均為大寫開頭）。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RuntimeConfigJson {
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(rename = "User")]
    pub user: Option<String>,
    /// map 形式（值為空物件），只取 keys；用 BTreeMap 保證排序確定性。
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RootFsJson {
    pub diff_ids: Vec<String>,
}

/// 偵測 `extracted_root`（已解開的 image tar）的格式並依 `platform` 解析。
pub(crate) fn resolve_image(extracted_root: &Path, platform: &str) -> Result<ResolvedImage> {
    let index_path = extracted_root.join("index.json");
    let blobs_dir = extracted_root.join("blobs");
    if index_path.is_file() && blobs_dir.is_dir() {
        return resolve_oci(extracted_root, platform);
    }
    let docker_manifest = extracted_root.join("manifest.json");
    if docker_manifest.is_file() {
        return resolve_docker(extracted_root, platform);
    }
    bail!(
        "不是有效的 image archive：找不到 index.json + blobs/（OCI layout）也找不到 manifest.json（docker-archive）；\
         請以 `docker save -o <file> <image>` 或 `podman save --format oci-archive` 重新匯出"
    )
}

fn split_platform(p: &str) -> Result<(&str, &str)> {
    let Some((os, arch)) = p.split_once('/') else {
        bail!("platform 格式錯誤（應為 os/arch，例如 linux/amd64）：{p}");
    };
    Ok((os, arch))
}

fn no_platform_help(platform: &str, available: &BTreeSet<String>) -> String {
    let list = if available.is_empty() {
        "（無）".to_string()
    } else {
        available.iter().cloned().collect::<Vec<_>>().join("、")
    };
    format!(
        "image archive 中找不到平台 {platform} 的 image；可用平台：{list}。\
         請改用 `docker save --platform {platform}` 或 `docker buildx build --platform {platform}` 匯出含該平台的 image，\
         或修改 appcipe.yml 的 image.platform"
    )
}

// ---------- OCI layout ----------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OciIndex {
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OciDescriptor {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    digest: String,
    platform: Option<OciPlatform>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OciPlatform {
    architecture: Option<String>,
    os: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OciManifest {
    config: OciDescriptor,
    #[serde(default)]
    layers: Vec<OciDescriptor>,
}

/// 由 digest（`sha256:<64hex>`）取得 blob 檔案路徑；嚴格驗證避免路徑注入。
fn blob_path(root: &Path, digest: &str) -> Result<PathBuf> {
    if digest.is_empty() {
        bail!(
            "image archive 的 index/manifest descriptor 缺少 digest 欄位；archive 可能損毀或非標準，請重新匯出"
        );
    }
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!("不支援的 digest 演算法（僅支援 sha256）：{digest}");
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("digest 格式錯誤（應為 sha256: 加 64 位十六進位）：{digest}");
    }
    let p = root.join("blobs").join("sha256").join(hex);
    if !p.is_file() {
        bail!("image archive 缺少 blob：blobs/sha256/{hex}；archive 可能不完整，請重新匯出");
    }
    Ok(p)
}

fn is_index_media_type(mt: Option<&str>) -> bool {
    matches!(mt, Some(s) if s.contains("image.index") || s.contains("manifest.list"))
}

/// 展開（可能巢狀的）index，收集所有 image manifest 描述子。
fn collect_manifest_descriptors(
    root: &Path,
    descs: &[OciDescriptor],
    out: &mut Vec<OciDescriptor>,
    depth: usize,
) -> Result<()> {
    if depth > 4 {
        bail!("OCI index 巢狀層級過深（>4）；image archive 可能已損毀");
    }
    for d in descs {
        if is_index_media_type(d.media_type.as_deref()) {
            let p = blob_path(root, &d.digest)?;
            let bytes = fs::read(&p)?;
            let nested: OciIndex = serde_json::from_slice(&bytes)
                .with_context(|| format!("解析巢狀 index blob 失敗：{}", d.digest))?;
            collect_manifest_descriptors(root, &nested.manifests, out, depth + 1)?;
        } else {
            out.push(d.clone());
        }
    }
    Ok(())
}

fn load_oci_manifest(root: &Path, digest: &str) -> Result<OciManifest> {
    let p = blob_path(root, digest)?;
    let bytes = fs::read(&p)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("解析 image manifest blob 失敗：{digest}"))
}

fn load_image_config(root: &Path, digest: &str) -> Result<ImageConfigJson> {
    let p = blob_path(root, digest)?;
    let bytes = fs::read(&p)?;
    serde_json::from_slice(&bytes).with_context(|| format!("解析 image config blob 失敗：{digest}"))
}

fn resolve_oci(root: &Path, platform: &str) -> Result<ResolvedImage> {
    let (want_os, want_arch) = split_platform(platform)?;

    let index_bytes = fs::read(root.join("index.json"))?;
    let index: OciIndex = serde_json::from_slice(&index_bytes).context("解析 index.json 失敗")?;

    let mut manifests = Vec::new();
    collect_manifest_descriptors(root, &index.manifests, &mut manifests, 0)?;
    if manifests.is_empty() {
        bail!("index.json 中沒有任何 image manifest；image archive 可能已損毀");
    }

    let mut available: BTreeSet<String> = BTreeSet::new();
    let mut chosen: Option<(OciManifest, ImageConfigJson)> = None;

    for d in &manifests {
        if let Some(p) = &d.platform {
            // 有宣告 platform：直接比對 os/architecture。
            let os = p.os.as_deref().unwrap_or("");
            let arch = p.architecture.as_deref().unwrap_or("");
            if os == "unknown" && arch == "unknown" {
                // buildx 的 attestation manifest，略過。
                continue;
            }
            if os == want_os && arch == want_arch {
                let m = load_oci_manifest(root, &d.digest)?;
                let cfg = load_image_config(root, &m.config.digest)?;
                chosen = Some((m, cfg));
                break;
            }
            available.insert(format!("{os}/{arch}"));
        } else {
            // 未宣告 platform（單平台 docker save 常見）：讀 config 判斷。
            let m = load_oci_manifest(root, &d.digest)?;
            let cfg = load_image_config(root, &m.config.digest)?;
            let os = cfg.os.clone().unwrap_or_default();
            let arch = cfg.architecture.clone().unwrap_or_default();
            if os == want_os && arch == want_arch {
                chosen = Some((m, cfg));
                break;
            }
            available.insert(format!("{os}/{arch}"));
        }
    }

    let Some((manifest, config)) = chosen else {
        bail!("{}", no_platform_help(platform, &available));
    };

    let layers = manifest
        .layers
        .iter()
        .map(|l| blob_path(root, &l.digest))
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolvedImage { config, layers })
}

// ---------- 傳統 docker-archive ----------

#[derive(Debug, Deserialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(default, rename = "Layers")]
    layers: Vec<String>,
}

/// 將 manifest.json 內的相對路徑安全地映射到解壓根目錄。
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let r = sanitize_rel_path(Path::new(rel))
        .with_context(|| format!("docker-archive manifest 內含不安全路徑：{rel}"))?;
    let p = root.join(r);
    if !p.is_file() {
        bail!("image archive 缺少檔案：{rel}；archive 可能不完整，請重新匯出");
    }
    Ok(p)
}

fn resolve_docker(root: &Path, platform: &str) -> Result<ResolvedImage> {
    let (want_os, want_arch) = split_platform(platform)?;

    let bytes = fs::read(root.join("manifest.json"))?;
    let entries: Vec<DockerManifestEntry> = serde_json::from_slice(&bytes)
        .context("解析 docker-archive 的 manifest.json 失敗（應為 JSON 陣列）")?;
    if entries.is_empty() {
        bail!("docker-archive 的 manifest.json 是空陣列；image archive 可能已損毀");
    }

    let mut available: BTreeSet<String> = BTreeSet::new();
    for entry in &entries {
        let cfg_path = safe_join(root, &entry.config)?;
        let cfg_bytes = fs::read(&cfg_path)?;
        let cfg: ImageConfigJson = serde_json::from_slice(&cfg_bytes)
            .with_context(|| format!("解析 image config 失敗：{}", entry.config))?;
        let os = cfg.os.clone().unwrap_or_default();
        let arch = cfg.architecture.clone().unwrap_or_default();
        if os == want_os && arch == want_arch {
            let layers = entry
                .layers
                .iter()
                .map(|l| safe_join(root, l))
                .collect::<Result<Vec<_>>>()?;
            return Ok(ResolvedImage {
                config: cfg,
                layers,
            });
        }
        available.insert(format!("{os}/{arch}"));
    }
    bail!("{}", no_platform_help(platform, &available))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_rejects_bad_digests() {
        let root = Path::new(".");
        assert!(blob_path(root, "md5:abcd").is_err());
        assert!(blob_path(root, "sha256:../../evil").is_err());
        assert!(blob_path(root, "sha256:zz").is_err());
        // 長度正確但含非 hex 字元
        let bad = format!("sha256:{}", "g".repeat(64));
        assert!(blob_path(root, &bad).is_err());
    }

    #[test]
    fn index_media_type_detection() {
        assert!(is_index_media_type(Some(
            "application/vnd.oci.image.index.v1+json"
        )));
        assert!(is_index_media_type(Some(
            "application/vnd.docker.distribution.manifest.list.v2+json"
        )));
        assert!(!is_index_media_type(Some(
            "application/vnd.oci.image.manifest.v1+json"
        )));
        assert!(!is_index_media_type(None));
    }

    #[test]
    fn invalid_archive_root_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_image(tmp.path(), "linux/amd64").unwrap_err();
        assert!(format!("{err:#}").contains("不是有效的 image archive"));
    }
}
