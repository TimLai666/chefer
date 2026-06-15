//! 從自身單檔串流解壓 bundle。
//!
//! 管線（單趟串流，不把 payload 整包讀進記憶體；app 可能數 GB）：
//! `開自身檔案 → seek 到 footer.offset → take(length) → TeeReader（邊讀邊餵
//! sha256 hasher）→ zstd 解碼 → tar 解包到暫存目錄`。
//!
//! 安全性：
//! - tar entry 路徑拒絕絕對路徑、Windows 磁碟前綴與 `..`（見 [`sanitize_rel_path`]）。
//! - bundle tar 只應含一般檔案與目錄；其他類型（symlink 等）一律拒絕。
//! - sha256 驗證未通過前，解壓出的內容**絕不**交付執行；
//!   驗證失敗（或解壓中途出錯）時暫存目錄會自動刪除。

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use chefer_bundle::Footer;

/// 解壓選項。
pub struct ExtractOptions<'a> {
    /// 暫存目錄要建立在哪個父目錄下（`--extract-dir`；None = 系統 temp）。
    pub extract_parent: Option<&'a Path>,
    /// 是否保留暫存目錄（`--keep-tmp`；不隨程式結束刪除）。
    pub keep_tmp: bool,
}

/// 解壓結果。
///
/// `_tempdir` 為 `Some` 時，drop 會自動刪除整個暫存目錄；
/// `--keep-tmp` 時為 `None`（目錄已交由使用者保管，路徑會印在 log）。
#[derive(Debug)]
pub struct Extracted {
    _tempdir: Option<TempDir>,
    /// 解出的 bundle 目錄（內含 manifest.json）。
    pub bundle_dir: PathBuf,
}

/// 從 `exe` 的 `[ft.offset, ft.offset + ft.length)` 區段串流解壓 bundle 到暫存目錄。
///
/// 成功時回傳 [`Extracted`]；sha256 不符或內容損毀時報錯，
/// 且暫存目錄已刪除（即使指定了 `--keep-tmp` 也不保留未通過驗證的內容）。
///
/// 這是「不快取」路徑（`--no-cache` / `--keep-tmp`）；預設啟動走
/// [`extract_bundle_cached`]，跳過重複解壓。
pub fn extract_bundle(exe: &Path, ft: &Footer, opts: &ExtractOptions<'_>) -> Result<Extracted> {
    // 在驗證通過並決定 keep 之前，由 TempDir 負責出錯時的自動清理。
    let tempdir = match opts.extract_parent {
        Some(parent) => {
            fs_err::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create the directory specified by --extract-dir: {}",
                    parent.display()
                )
            })?;
            TempDir::with_prefix_in("chefer-", parent).with_context(|| {
                format!(
                    "failed to create a temp directory under {}",
                    parent.display()
                )
            })?
        }
        None => {
            TempDir::with_prefix("chefer-").context("failed to create the system temp directory")?
        }
    };

    // 解壓 + 驗證 + 佈局檢查；出錯時 tempdir 於此函式返回時 drop（內容刪除）。
    extract_payload_to(tempdir.path(), exe, ft)?;
    let bundle_dir = tempdir.path().join("bundle");

    if opts.keep_tmp {
        let root = tempdir.keep();
        tracing::info!("Temp directory kept (--keep-tmp): {}", root.display());
        Ok(Extracted {
            _tempdir: None,
            bundle_dir: root.join("bundle"),
        })
    } else {
        Ok(Extracted {
            _tempdir: Some(tempdir),
            bundle_dir,
        })
    }
}

/// 持久化 bundle 解壓快取：依 payload 的 sha256 為鍵。
///
/// 首次啟動把 bundle 解到 `<cache_root>/<sha256>/` 並寫入 `.complete` 標記；
/// 後續啟動（含其他 instance）若該目錄已存在且完整，**完全跳過開檔/解壓/sha 驗證**，
/// 直接重用——這是大 image 重複啟動的主要加速來源。內容雜湊當鍵，故同內容必命中、
/// 不同內容自然分流。快取命中代表先前已對「同一份內容」解壓並驗證過。
pub fn extract_bundle_cached(exe: &Path, ft: &Footer, cache_root: &Path) -> Result<Extracted> {
    let key = hex::encode(ft.sha256);
    let cache_dir = cache_root.join(&key);
    let bundle_dir = cache_dir.join("bundle");

    if is_complete_cache(&cache_dir) {
        tracing::info!("reusing cached bundle: {}", cache_dir.display());
        return Ok(Extracted {
            _tempdir: None,
            bundle_dir,
        });
    }

    // Miss：解到 cache_root 下的暫存目錄（同卷，rename 才是原子且免跨卷複製），
    // 驗證通過後寫 .complete，再原子 rename 成 <sha>/。出錯時 staging 自動清除。
    fs_err::create_dir_all(cache_root).with_context(|| {
        format!(
            "failed to create bundle cache dir: {}",
            cache_root.display()
        )
    })?;
    let staging = TempDir::with_prefix_in(".staging-", cache_root).with_context(|| {
        format!(
            "failed to create a staging dir under {}",
            cache_root.display()
        )
    })?;
    extract_payload_to(staging.path(), exe, ft)?;
    fs_err::write(staging.path().join(COMPLETE_MARKER), b"")
        .context("failed to write cache completion marker")?;

    // 促成：rename staging → <sha>/。若目標已存在（另一 instance 先完成），rename 失敗，
    // 改用既有的並刪掉自己的 staging。rename 是在 .complete 寫入「之後」才做，且為原子操作，
    // 故 cache_dir 一旦存在即代表完整。
    let staged = staging.keep();
    match fs_err::rename(&staged, &cache_dir) {
        Ok(()) => {}
        Err(e) => {
            let _ = fs_err::remove_dir_all(&staged);
            if !is_complete_cache(&cache_dir) {
                return Err(e).with_context(|| {
                    format!(
                        "failed to promote staged bundle into the cache: {}",
                        cache_dir.display()
                    )
                });
            }
            tracing::info!(
                "reusing cached bundle (won by a concurrent run): {}",
                cache_dir.display()
            );
        }
    }

    Ok(Extracted {
        _tempdir: None,
        bundle_dir,
    })
}

/// 快取目錄是否完整（已有 `.complete` 標記且含 manifest.json）。
fn is_complete_cache(cache_dir: &Path) -> bool {
    cache_dir.join(COMPLETE_MARKER).is_file()
        && chefer_bundle::layout::manifest_path(&cache_dir.join("bundle")).is_file()
}

/// 快取完成標記檔名（存在 = 該目錄是一次完整、已驗證的解壓）。
const COMPLETE_MARKER: &str = ".chefer-complete";

/// 平台預設的 bundle 解壓快取根目錄：
/// - Windows：`%LOCALAPPDATA%\chefer\cache\bundles`
/// - macOS：`~/Library/Caches/chefer/bundles`
/// - Linux／其他 unix：`$XDG_CACHE_HOME/chefer/bundles` 或 `~/.cache/chefer/bundles`
/// - 取不到上述環境時退回系統 temp 下的 `chefer-bundle-cache`
pub fn default_cache_root() -> PathBuf {
    let sub = |base: PathBuf| base.join("chefer").join("bundles");
    #[cfg(target_os = "windows")]
    {
        if let Some(v) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return sub(PathBuf::from(v).join("cache"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return sub(PathBuf::from(h).join("Library").join("Caches"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            return sub(PathBuf::from(x));
        }
        if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return sub(PathBuf::from(h).join(".cache"));
        }
    }
    std::env::temp_dir().join("chefer-bundle-cache")
}

/// 將 payload 串流解壓 + sha256 驗證 + 佈局檢查到 `dest`（呼叫端負責 `dest` 的清理）。
fn extract_payload_to(dest: &Path, exe: &Path, ft: &Footer) -> Result<()> {
    // 開自身檔案，限制在 payload 區段，邊讀邊算 sha256。
    let mut f = fs_err::File::open(exe)
        .with_context(|| format!("failed to open own executable: {}", exe.display()))?;
    f.seek(SeekFrom::Start(ft.offset))
        .with_context(|| format!("failed to seek to payload start (offset={})", ft.offset))?;
    let mut tee = TeeReader::new(f.take(ft.length));

    // 解壓（zstd → tar；非 zstd flag 時直接視為 tar）。
    if ft.is_zstd() {
        let decoder = zstd::stream::read::Decoder::new(&mut tee).context(
            "failed to create the zstd decoder (the start of the payload may be corrupted)",
        )?;
        // 解壓「輸出」總量限制器：以壓縮輸入長度的合理倍數為上限，擋下
        // 長檔名/pax 標頭撐爆 read_to_end 的解壓炸彈（此讀取發生在 sha 驗證之前）。
        // 非 zstd 路徑的輸出 = 輸入，已被 take(length) 自然限制，無需再包。
        let guarded =
            chefer_bundle::LimitedReader::new(decoder, chefer_bundle::bomb_limit_for(ft.length));
        unpack_tar(guarded, dest)?;
    } else {
        unpack_tar(&mut tee, dest)?;
    }

    // 排空剩餘 payload，讓 sha256 覆蓋完整的 footer.length。
    io::copy(&mut tee, &mut io::sink()).context("failed to read the end of the payload")?;
    let got = tee.finalize();
    if got != ft.sha256 {
        bail!(
            "payload SHA-256 verification failed (expected {}, got {}); \
             the file may be corrupted or tampered with, and the extracted content has been deleted. Please re-download or repack this single file",
            hex::encode(ft.sha256),
            hex::encode(got)
        );
    }

    // 確認佈局：tar 內路徑以 bundle/ 為根。
    let manifest_path = chefer_bundle::layout::manifest_path(&dest.join("bundle"));
    if !manifest_path.is_file() {
        bail!(
            "{} not found after extraction; the payload does not match the bundle layout (the tar should be rooted at bundle/). \
             Please repack with the same version of chefer",
            manifest_path.display()
        );
    }
    Ok(())
}

/// 串流解 tar 到 `dest`，每個 entry 先做路徑安全檢查再落地。
fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .context("failed to read tar contents (the payload may be corrupted)")?
    {
        let mut entry = entry.context("failed to read tar entry (the payload may be corrupted)")?;
        let raw = entry
            .path()
            .context("could not parse tar entry path")?
            .into_owned();
        let rel = sanitize_rel_path(&raw)?;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs_err::create_dir_all(dest.join(&rel))
                    .with_context(|| format!("failed to create directory: {}", rel.display()))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                let out = dest.join(&rel);
                if let Some(parent) = out.parent() {
                    fs_err::create_dir_all(parent).with_context(|| {
                        format!("failed to create directory: {}", parent.display())
                    })?;
                }
                let mut file = fs_err::File::create(&out)
                    .with_context(|| format!("failed to create file: {}", out.display()))?;
                io::copy(&mut entry, &mut file)
                    .with_context(|| format!("failed to write file: {}", out.display()))?;
            }
            other => {
                bail!(
                    "bundle tar contains an unsupported entry type {:?}: {}; \
                     a bundle should only contain regular files and directories, please repack",
                    other,
                    raw.display()
                );
            }
        }
    }
    Ok(())
}

/// 把 tar entry 路徑轉成「相對、無越界」的安全路徑。
/// 拒絕：絕對路徑、Windows 磁碟前綴、`..`、片段中含 `:` 或 `\`。
fn sanitize_rel_path(p: &Path) -> Result<PathBuf> {
    let mut buf = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                bail!(
                    "bundle tar contains an absolute path or Windows drive prefix; refusing to extract: {}",
                    p.display()
                );
            }
            Component::CurDir => {}
            Component::ParentDir => {
                bail!(
                    "bundle tar contains a `..` path; refusing to extract: {}",
                    p.display()
                );
            }
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains(':') || s.contains('\\') {
                    bail!(
                        "bundle tar path segment contains an illegal character (`:` or `\\`): {s}"
                    );
                }
                buf.push(seg);
            }
        }
    }
    Ok(buf)
}

/// tee reader：讀取內層 reader 的同時把讀到的 bytes 餵給 SHA-256 hasher。
struct TeeReader<R: Read> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> TeeReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finalize(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl<R: Read> Read for TeeReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// 手工組 payload：zstd(tar(bundle/manifest.json 假檔))。
    fn build_payload(manifest_content: &[u8]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_buf);

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            h.set_mtime(0);
            b.append_data(&mut h, "bundle/", io::empty()).unwrap();

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_size(manifest_content.len() as u64);
            h.set_mtime(0);
            b.append_data(&mut h, "bundle/manifest.json", manifest_content)
                .unwrap();

            b.finish().unwrap();
        }
        zstd::stream::encode_all(&tar_buf[..], 3).unwrap()
    }

    /// 寫出假單檔：隨機前綴 bytes + payload + footer（用 chefer_bundle::Footer 組）。
    fn write_fake_single(path: &Path, prefix_len: usize, payload: &[u8]) -> Footer {
        let sha: [u8; 32] = Sha256::digest(payload).into();
        let ft = Footer::new_zstd(prefix_len as u64, payload.len() as u64, sha);
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![0xABu8; prefix_len]).unwrap();
        f.write_all(payload).unwrap();
        f.write_all(&ft.to_bytes()).unwrap();
        ft
    }

    #[test]
    fn extract_success_streaming() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-app.exe");
        let manifest_content = br#"{"fake": true}"#;
        let payload = build_payload(manifest_content);
        let ft = write_fake_single(&exe, 1234, &payload);

        let parent = tmp.path().join("work");
        let opts = ExtractOptions {
            extract_parent: Some(&parent),
            keep_tmp: false,
        };
        let extracted = extract_bundle(&exe, &ft, &opts).unwrap();
        let got = fs::read(extracted.bundle_dir.join("manifest.json")).unwrap();
        assert_eq!(got, manifest_content);

        // drop 後暫存目錄應清掉（parent 內不留任何子目錄）
        let bundle_dir = extracted.bundle_dir.clone();
        drop(extracted);
        assert!(!bundle_dir.exists(), "未指定 --keep-tmp 時應自動刪除暫存");
    }

    #[test]
    fn keep_tmp_preserves_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-app.exe");
        let payload = build_payload(b"{}");
        let ft = write_fake_single(&exe, 64, &payload);

        let parent = tmp.path().join("kept");
        let opts = ExtractOptions {
            extract_parent: Some(&parent),
            keep_tmp: true,
        };
        let extracted = extract_bundle(&exe, &ft, &opts).unwrap();
        let bundle_dir = extracted.bundle_dir.clone();
        drop(extracted);
        assert!(
            bundle_dir.join("manifest.json").is_file(),
            "--keep-tmp 時 drop 後目錄應仍存在"
        );
    }

    #[test]
    fn tampered_payload_fails_and_cleans_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-app.exe");
        let mut payload = build_payload(b"{}");
        // footer 以原始 payload 計算 sha，之後竄改 payload 中段一個 byte。
        let sha: [u8; 32] = Sha256::digest(&payload).into();
        let mid = payload.len() / 2;
        payload[mid] ^= 0xFF;
        let ft = Footer::new_zstd(64, payload.len() as u64, sha);
        let mut f = fs::File::create(&exe).unwrap();
        f.write_all(&[0xABu8; 64]).unwrap();
        f.write_all(&payload).unwrap();
        f.write_all(&ft.to_bytes()).unwrap();
        drop(f);

        let parent = tmp.path().join("work");
        let opts = ExtractOptions {
            extract_parent: Some(&parent),
            keep_tmp: true, // 即使要求保留，驗證失敗也必須刪除
        };
        let err = extract_bundle(&exe, &ft, &opts);
        assert!(err.is_err(), "竄改 payload 後必須報錯");

        // 暫存目錄必須已清乾淨（parent 下不留任何項目）
        let leftovers: Vec<_> = fs::read_dir(&parent).unwrap().collect();
        assert!(
            leftovers.is_empty(),
            "驗證失敗後暫存目錄必須刪除，剩餘：{leftovers:?}"
        );
    }

    #[test]
    fn rejects_path_escape_entries() {
        // tar 內含 `..` 路徑 → 拒絕解壓。
        // tar::Builder 的 append_data 會自行拒絕 `..`，因此直接寫 raw header
        // 模擬惡意 tar。
        let mut tar_buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_buf);
            let content = b"evil";
            let mut h = tar::Header::new_gnu();
            {
                let gnu = h.as_gnu_mut().unwrap();
                let name = b"bundle/../evil.txt";
                gnu.name[..name.len()].copy_from_slice(name);
            }
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_size(content.len() as u64);
            h.set_mtime(0);
            h.set_cksum();
            b.append(&h, &content[..]).unwrap();
            b.finish().unwrap();
        }
        let payload = zstd::stream::encode_all(&tar_buf[..], 3).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-app.exe");
        let ft = write_fake_single(&exe, 16, &payload);
        let opts = ExtractOptions {
            extract_parent: None,
            keep_tmp: false,
        };
        let err = extract_bundle(&exe, &ft, &opts).unwrap_err();
        assert!(
            format!("{err:#}").contains(".."),
            "錯誤應指出 `..`：{err:#}"
        );
    }

    #[test]
    fn cached_extract_reuses_and_skips_on_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("fake-app.exe");
        let manifest_content = br#"{"cached": true}"#;
        let payload = build_payload(manifest_content);
        let ft = write_fake_single(&exe, 100, &payload);
        let cache_root = tmp.path().join("cache");

        // 第一次：miss → 解壓並促成快取。
        let first = extract_bundle_cached(&exe, &ft, &cache_root).unwrap();
        assert_eq!(
            fs::read(first.bundle_dir.join("manifest.json")).unwrap(),
            manifest_content
        );
        let cache_dir = cache_root.join(hex::encode(ft.sha256));
        assert!(cache_dir.join(COMPLETE_MARKER).is_file(), "應寫入完成標記");
        // 快取路徑不隨 drop 刪除（持久）。
        let bundle_dir = first.bundle_dir.clone();
        drop(first);
        assert!(bundle_dir.join("manifest.json").is_file(), "快取應保留");

        // 第二次：把 exe 的 payload 整段毀掉——若仍成功，證明命中時根本沒重讀/解壓。
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().write(true).open(&exe).unwrap();
            f.seek(SeekFrom::Start(ft.offset)).unwrap();
            f.write_all(&vec![0u8; payload.len()]).unwrap();
        }
        let second = extract_bundle_cached(&exe, &ft, &cache_root).unwrap();
        assert_eq!(
            fs::read(second.bundle_dir.join("manifest.json")).unwrap(),
            manifest_content,
            "命中快取應重用,毋須重讀 payload"
        );
    }

    #[test]
    fn sanitize_rejects_absolute_and_parent() {
        assert!(sanitize_rel_path(Path::new("bundle/manifest.json")).is_ok());
        assert!(sanitize_rel_path(Path::new("./bundle/a")).is_ok());
        assert!(sanitize_rel_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_rel_path(Path::new("a/../b")).is_err());
        #[cfg(windows)]
        assert!(sanitize_rel_path(Path::new(r"C:\evil")).is_err());
    }
}
