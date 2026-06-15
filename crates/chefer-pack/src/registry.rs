//! 從 OCI registry 直接拉取 image（`source: image`）。
//!
//! 用 `oci-client`（rustls）依 service 的 `platform` 拉 manifest + config + layers，產出與
//! [`crate::image::resolve_image`] 相同的 [`ResolvedImage`]（config + 各層 blob 的暫存檔路徑），
//! 之後接進與本機 tar 完全相同的 repack 流程。pack 是同步的，故在此自建一個 tokio runtime
//! `block_on` async 拉取。v1 僅匿名拉取（公開 image）；reference 的合法性與「禁 latest／須釘版」
//! 由 appcipe-spec 的驗證把關。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oci_client::client::{ClientConfig, ImageData};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

use crate::image::{ImageConfigJson, ResolvedImage};

/// 可接受的 layer media type（repack 端能處理 gzip / zstd / 未壓縮 tar）。
const ACCEPTED_LAYER_MEDIA_TYPES: &[&str] = &[
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+zstd",
    "application/vnd.docker.image.rootfs.diff.tar.zstd",
];

/// 拉取 `reference` 在 `platform` 上的 image，layer blob 寫進 `tmp`（呼叫端負責保留其生命週期）。
pub(crate) fn pull_image(reference: &str, platform: &str, tmp: &Path) -> Result<ResolvedImage> {
    let (want_os, want_arch) = split_platform(platform)?;
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("無法解析 image reference：{reference}"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("建立 tokio runtime 失敗")?;

    let image: ImageData = rt.block_on(async {
        // 多架構（manifest index）時依 os/arch 挑選；單架構則此 resolver 不被使用。
        let resolver = {
            let (os, arch) = (want_os.clone(), want_arch.clone());
            move |entries: &[oci_client::manifest::ImageIndexEntry]| -> Option<String> {
                entries
                    .iter()
                    .find(|e| {
                        e.platform
                            .as_ref()
                            .is_some_and(|p| p.os == os && p.architecture == arch)
                    })
                    .map(|e| e.digest.clone())
            }
        };
        let config = ClientConfig {
            platform_resolver: Some(Box::new(resolver)),
            ..Default::default()
        };
        let client = Client::new(config);
        client
            .pull(
                &reference,
                &RegistryAuth::Anonymous,
                ACCEPTED_LAYER_MEDIA_TYPES.to_vec(),
            )
            .await
            .context("從 registry 拉取失敗（公開 image 採匿名拉取；私有 registry 尚未支援）")
    })?;

    if image.layers.is_empty() {
        bail!("registry 回傳的 image 沒有任何 layer；reference 或平台可能有誤");
    }

    let config: ImageConfigJson =
        serde_json::from_slice(&image.config.data).context("解析 registry image config 失敗")?;
    // 防呆：確認拉到的就是要的平台（單架構 image 沒有 index、resolver 不生效，這裡再核對一次）。
    if let (Some(os), Some(arch)) = (config.os.as_deref(), config.architecture.as_deref())
        && (os != want_os || arch != want_arch)
    {
        bail!(
            "registry image 平台為 {os}/{arch}，與要求的 {platform} 不符；\
             該 reference 可能不是多架構、或不含此平台，請改用對應平台的 image 或調整 image.platform"
        );
    }

    let mut layers = Vec::with_capacity(image.layers.len());
    for (idx, layer) in image.layers.iter().enumerate() {
        let p = tmp.join(format!("layer-{idx}.blob"));
        std::fs::write(&p, &layer.data)
            .with_context(|| format!("寫入第 {idx} 層 blob 失敗：{}", p.display()))?;
        layers.push(p);
    }
    // oci-client 以 buffer_unordered 平行下載 → 回傳的 layers **順序非 manifest 序**；
    // 依 config 的 rootfs.diff_ids 重排，確保與後續 repack 的逐層 diff_id 檢查對齊。
    let layers = order_layers_by_diff_ids(layers, &config)?;
    Ok(ResolvedImage { config, layers })
}

/// 把（順序不定的）已下載 layer 依 `config.rootfs.diff_ids` 的順序重排。
fn order_layers_by_diff_ids(
    layers: Vec<PathBuf>,
    config: &ImageConfigJson,
) -> Result<Vec<PathBuf>> {
    let want = config
        .rootfs
        .as_ref()
        .map(|r| r.diff_ids.as_slice())
        .unwrap_or(&[]);
    if want.is_empty() {
        return Ok(layers); // 無 diff_ids 資訊：維持原序（後續仍會做逐層比對）。
    }
    if want.len() != layers.len() {
        bail!(
            "registry image 層數（{}）與 config diff_ids（{}）不一致；image 可能不完整",
            layers.len(),
            want.len()
        );
    }
    // 先算每個下載 layer 的 diff_id（解壓後 sha256），再依 want 順序挑出（remove 處理重複層）。
    let mut pairs: Vec<(String, PathBuf)> = Vec::with_capacity(layers.len());
    for p in layers {
        let d = crate::layers::compute_diff_id(&p)?;
        pairs.push((d, p));
    }
    let mut ordered = Vec::with_capacity(want.len());
    for w in want {
        let Some(pos) = pairs.iter().position(|(d, _)| d == w) else {
            bail!("拉到的層找不到對應 diff_id：{w}（registry 回應可能不完整）");
        };
        ordered.push(pairs.remove(pos).1);
    }
    Ok(ordered)
}

fn split_platform(p: &str) -> Result<(String, String)> {
    let Some((os, arch)) = p.split_once('/') else {
        bail!("platform 格式錯誤（應為 os/arch，例如 linux/amd64）：{p}");
    };
    Ok((os.to_string(), arch.to_string()))
}
