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

/// 從 `exe` 的 `[ft.offset, ft.offset + ft.length)` 區段串流解壓 bundle。
///
/// 成功時回傳 [`Extracted`]；sha256 不符或內容損毀時報錯，
/// 且暫存目錄已刪除（即使指定了 `--keep-tmp` 也不保留未通過驗證的內容）。
pub fn extract_bundle(exe: &Path, ft: &Footer, opts: &ExtractOptions<'_>) -> Result<Extracted> {
    // ── 1. 準備暫存目錄 ─────────────────────────────────────────
    // 在驗證通過並決定 keep 之前，由 TempDir 負責出錯時的自動清理。
    let tempdir = match opts.extract_parent {
        Some(parent) => {
            fs_err::create_dir_all(parent).with_context(|| {
                format!("建立 --extract-dir 指定的目錄失敗：{}", parent.display())
            })?;
            TempDir::with_prefix_in("chefer-", parent)
                .with_context(|| format!("在 {} 下建立暫存目錄失敗", parent.display()))?
        }
        None => TempDir::with_prefix("chefer-").context("建立系統暫存目錄失敗")?,
    };

    // ── 2. 開自身檔案，限制在 payload 區段，邊讀邊算 sha256 ─────
    let mut f = fs_err::File::open(exe)
        .with_context(|| format!("開啟自身執行檔失敗：{}", exe.display()))?;
    f.seek(SeekFrom::Start(ft.offset))
        .with_context(|| format!("seek 到 payload 起點（offset={}）失敗", ft.offset))?;
    let mut tee = TeeReader::new(f.take(ft.length));

    // ── 3. 解壓（zstd → tar；非 zstd flag 時直接視為 tar）────────
    let dest = tempdir.path();
    if ft.is_zstd() {
        let decoder = zstd::stream::read::Decoder::new(&mut tee)
            .context("建立 zstd 解碼器失敗（payload 開頭可能已損毀）")?;
        // 解壓「輸出」總量限制器：以壓縮輸入長度的合理倍數為上限，擋下
        // 長檔名/pax 標頭撐爆 read_to_end 的解壓炸彈（此讀取發生在 sha 驗證之前）。
        // 非 zstd 路徑的輸出 = 輸入，已被 take(length) 自然限制，無需再包。
        let guarded =
            chefer_bundle::LimitedReader::new(decoder, chefer_bundle::bomb_limit_for(ft.length));
        unpack_tar(guarded, dest)?;
    } else {
        unpack_tar(&mut tee, dest)?;
    }

    // ── 4. 排空剩餘 payload，讓 sha256 覆蓋完整的 footer.length ──
    // tar 在結尾零區塊後即停止讀取，zstd 解碼器也可能殘留未消化的
    // bytes；這裡把剩餘區段讀完（只進 hasher，不佔記憶體）。
    io::copy(&mut tee, &mut io::sink()).context("讀取 payload 尾端失敗")?;
    let got = tee.finalize();
    if got != ft.sha256 {
        // 直接返回錯誤：tempdir 在此 drop，解壓內容隨之刪除。
        bail!(
            "payload 的 SHA-256 驗證失敗（預期 {}，實際 {}）；\
             檔案可能已損毀或遭竄改，解壓內容已刪除。請重新下載或重新打包此單檔",
            hex::encode(ft.sha256),
            hex::encode(got)
        );
    }

    // ── 5. 確認佈局：tar 內路徑以 bundle/ 為根 ───────────────────
    let bundle_dir = dest.join("bundle");
    let manifest_path = chefer_bundle::layout::manifest_path(&bundle_dir);
    if !manifest_path.is_file() {
        bail!(
            "解壓後找不到 {}；payload 內容不符合 bundle 佈局（tar 應以 bundle/ 為根）。\
             請以相同版本的 chefer 重新打包",
            manifest_path.display()
        );
    }

    // ── 6. --keep-tmp：真正保留目錄並印出路徑 ────────────────────
    if opts.keep_tmp {
        let root = tempdir.keep();
        tracing::info!("已保留暫存目錄（--keep-tmp）：{}", root.display());
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

/// 串流解 tar 到 `dest`，每個 entry 先做路徑安全檢查再落地。
fn unpack_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .context("讀取 tar 內容失敗（payload 可能損毀）")?
    {
        let mut entry = entry.context("讀取 tar 項目失敗（payload 可能損毀）")?;
        let raw = entry.path().context("tar 項目路徑無法解析")?.into_owned();
        let rel = sanitize_rel_path(&raw)?;
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs_err::create_dir_all(dest.join(&rel))
                    .with_context(|| format!("建立目錄失敗：{}", rel.display()))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                let out = dest.join(&rel);
                if let Some(parent) = out.parent() {
                    fs_err::create_dir_all(parent)
                        .with_context(|| format!("建立目錄失敗：{}", parent.display()))?;
                }
                let mut file = fs_err::File::create(&out)
                    .with_context(|| format!("建立檔案失敗：{}", out.display()))?;
                io::copy(&mut entry, &mut file)
                    .with_context(|| format!("寫入檔案失敗：{}", out.display()))?;
            }
            other => {
                bail!(
                    "bundle tar 內含不支援的項目類型 {:?}：{}；\
                     bundle 只應包含一般檔案與目錄，請重新打包",
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
                    "bundle tar 內含絕對路徑或 Windows 磁碟前綴，拒絕解壓：{}",
                    p.display()
                );
            }
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("bundle tar 內含 `..` 路徑，拒絕解壓：{}", p.display());
            }
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains(':') || s.contains('\\') {
                    bail!("bundle tar 路徑片段含非法字元（`:` 或 `\\`）：{s}");
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
    fn sanitize_rejects_absolute_and_parent() {
        assert!(sanitize_rel_path(Path::new("bundle/manifest.json")).is_ok());
        assert!(sanitize_rel_path(Path::new("./bundle/a")).is_ok());
        assert!(sanitize_rel_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_rel_path(Path::new("a/../b")).is_err());
        #[cfg(windows)]
        assert!(sanitize_rel_path(Path::new(r"C:\evil")).is_err());
    }
}
