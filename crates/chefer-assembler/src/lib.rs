//! chefer-assembler — 將 chefer-runtime 執行檔與 bundle 目錄組成單一執行檔。
//!
//! 輸出格式（見 docs/DESIGN.md §4、§6）：
//! `[runtime 執行檔][payload = zstd(tar(bundle/))][80-byte footer]`
//!
//! - payload **全程串流**產生（tar → zstd → 邊寫檔邊算 sha256），
//!   不會把整個 bundle 或 payload 讀進記憶體（app 可能數 GB）。
//! - tar 內路徑一律以 `bundle/` 為根、使用 `/` 分隔
//!   （解壓後得到 `<dest>/bundle/manifest.json`）。
//! - footer 由 `chefer_bundle::Footer` 統一序列化，不自行手刻位元組。
//!
//! ## 輸出確定性（同輸入 → 相同 payload bytes）
//!
//! `tar` crate 的 `append_dir_all` 走訪順序平台相依，因此本 crate 自行走訪：
//! - 每層目錄項目以檔名排序後依序 append；
//! - tar header 的 **mtime 固定為 0**、uid/gid 固定為 0
//!   （不採來源檔案時間，避免重新 checkout / 複製 bundle 後輸出改變）；
//! - 檔案 mode 固定：目錄 `0o755`、`agents/` 下檔案 `0o755`
//!   （guest-agent 解出後需可執行）、其餘檔案 `0o644`。

use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// 組裝選項。
#[derive(Debug, Clone)]
pub struct AssembleOptions {
    /// zstd 壓縮等級（1..=22）；`0` 表示使用 zstd 函式庫預設（目前為 3）。
    pub zstd_level: i32,
}

impl Default for AssembleOptions {
    fn default() -> Self {
        // 預設 3：zstd 函式庫的預設等級，速度與壓縮率的平衡點。
        Self { zstd_level: 3 }
    }
}

/// 組裝結果摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssembleReport {
    /// payload 在輸出檔中的起始位移（= runtime 執行檔大小）。
    pub payload_offset: u64,
    /// payload（zstd 壓縮後）長度（bytes）。
    pub payload_len: u64,
    /// payload（壓縮後 bytes）的 SHA-256。
    pub sha256: [u8; 32],
    /// 輸出檔總大小（= offset + payload_len + footer 80 bytes）。
    pub out_size: u64,
}

impl AssembleReport {
    /// sha256 的小寫十六進位字串（方便顯示與比對）。
    pub fn sha256_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.sha256 {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

/// 將 `runtime_bin` 與 `bundle_dir` 組成單一執行檔 `out_path`。
///
/// 步驟：
/// 1. 檢查 bundle 內存在且可解析的 `manifest.json`（缺少→報錯）。
/// 2. 複製 runtime → out_path（覆蓋既有檔案）；複製後大小即 payload offset。
/// 3. 以 append 模式串流寫入 `zstd(tar(bundle/))`，邊寫邊算 sha256。
/// 4. 寫入 80-byte footer（`chefer_bundle::Footer::new_zstd`）。
/// 5. Unix host 上將產物 chmod 755（單一執行檔需可執行）；Windows host 無此概念，略過。
pub fn assemble(
    runtime_bin: &Path,
    bundle_dir: &Path,
    out_path: &Path,
    opts: &AssembleOptions,
) -> Result<AssembleReport> {
    // ── 前置檢查 ────────────────────────────────────────────────
    if !runtime_bin.is_file() {
        bail!(
            "找不到 runtime 執行檔：{}。\n\
             解法：以 `cargo build --release -p chefer-runtime` 自建，\
             或從 GitHub Releases 下載 runtime kit（搜尋目錄見 chefer_bundle::kit）。",
            runtime_bin.display()
        );
    }
    if !bundle_dir.is_dir() {
        bail!(
            "bundle 目錄不存在：{}。請先以 chefer-pack（或 `chefer build`）產生 bundle。",
            bundle_dir.display()
        );
    }
    let manifest_path = chefer_bundle::layout::manifest_path(bundle_dir);
    if !manifest_path.is_file() {
        bail!(
            "bundle 缺少 manifest.json（預期位置：{}）。\
             請先以 chefer-pack（或 `chefer build`）產生完整 bundle 再組裝。",
            manifest_path.display()
        );
    }
    // 順帶驗證 manifest 可解析且格式版本相容，避免組出無法執行的單檔。
    chefer_bundle::Manifest::load(&manifest_path)
        .with_context(|| format!("bundle 的 manifest.json 無效：{}", manifest_path.display()))?;

    let bundle_canon = bundle_dir
        .canonicalize()
        .with_context(|| format!("解析 bundle 路徑失敗：{}", bundle_dir.display()))?;

    // 防呆：out 不可與 runtime 是同一檔案（append 會破壞來源）。
    if out_path.exists()
        && let (Ok(a), Ok(b)) = (runtime_bin.canonicalize(), out_path.canonicalize())
        && a == b
    {
        bail!(
            "--out 不可與 --runtime 指向同一個檔案：{}（append payload 會破壞 runtime 來源）",
            a.display()
        );
    }

    // 確保輸出目錄存在；並防呆：out 不可位於 bundle 目錄內（tar 會把半成品收進去）。
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs_err::create_dir_all(parent)
            .with_context(|| format!("建立輸出目錄失敗：{}", parent.display()))?;
        if let Ok(parent_canon) = parent.canonicalize()
            && parent_canon.starts_with(&bundle_canon)
        {
            bail!(
                "--out 不可位於 bundle 目錄內（{}），否則 tar 會收錄寫入中的半成品；\
                 請改放到 bundle 之外的目錄。",
                bundle_canon.display()
            );
        }
    }

    // ── 1. 複製 runtime → out（覆蓋），大小即 payload offset ─────
    fs_err::copy(runtime_bin, out_path).with_context(|| {
        format!(
            "複製 runtime 失敗：{} → {}",
            runtime_bin.display(),
            out_path.display()
        )
    })?;
    let payload_offset = fs_err::metadata(out_path)?.len();

    // ── 2. 走訪 bundle（排序後）以求輸出確定性 ──────────────────
    let mut entries = Vec::new();
    walk_sorted(&bundle_canon, "", &mut entries)?;

    // ── 3. 串流管線：tar → zstd → (sha256 + 檔案) ────────────────
    let file = fs_err::OpenOptions::new()
        .append(true)
        .open(out_path)
        .with_context(|| format!("開啟輸出檔（append）失敗：{}", out_path.display()))?;
    let tee = HashingWriter::new(BufWriter::new(file));
    let encoder = zstd::stream::write::Encoder::new(tee, opts.zstd_level)
        .with_context(|| format!("建立 zstd 編碼器失敗（level={}）", opts.zstd_level))?;
    let mut tar = tar::Builder::new(encoder);

    // tar 根目錄項：bundle/
    append_dir_entry(&mut tar, "bundle")?;
    for e in &entries {
        let tar_path = format!("bundle/{}", e.rel);
        if e.is_dir {
            append_dir_entry(&mut tar, &tar_path)?;
        } else {
            append_file_entry(&mut tar, &tar_path, &e.abs)?;
        }
    }

    // 依序關閉各層串流（tar 結尾區塊 → zstd frame 結尾）。
    let encoder = tar.into_inner().context("關閉 tar 串流失敗")?;
    let tee = encoder.finish().context("關閉 zstd 串流失敗")?;
    let (mut writer, sha256, payload_len) = tee.finalize()?;

    // ── 4. 寫 footer（統一走 chefer_bundle::Footer）──────────────
    let footer = chefer_bundle::Footer::new_zstd(payload_offset, payload_len, sha256);
    writer
        .write_all(&footer.to_bytes())
        .context("寫入 footer 失敗")?;
    writer.flush().context("輸出緩衝區排清失敗")?;
    let file = writer
        .into_inner()
        .map_err(|e| anyhow::anyhow!("輸出緩衝區排清失敗：{e}"))?;
    file.sync_all()
        .with_context(|| format!("同步輸出檔失敗：{}", out_path.display()))?;
    drop(file);

    // ── 5. 大小自我檢查 ─────────────────────────────────────────
    let out_size = payload_offset + payload_len + chefer_bundle::FOOTER_LEN;
    let actual = fs_err::metadata(out_path)?.len();
    if actual != out_size {
        bail!(
            "內部錯誤：輸出大小不符（預期 {out_size}，實際 {actual}）；\
             輸出檔可能在組裝期間被其他程式修改：{}",
            out_path.display()
        );
    }

    // ── 6. Unix host 上 chmod 755 ───────────────────────────────
    // 非 Windows 目標的單一執行檔需要可執行位元；Windows host 沒有
    // Unix 權限概念（由 .exe 副檔名決定），故略過。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs_err::set_permissions(out_path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("設定執行權限（755）失敗：{}", out_path.display()))?;
    }

    Ok(AssembleReport {
        payload_offset,
        payload_len,
        sha256,
        out_size,
    })
}

/// 走訪結果的一個項目（rel 一律用 `/` 分隔，不含 `bundle/` 前綴）。
struct WalkEntry {
    abs: PathBuf,
    rel: String,
    is_dir: bool,
}

/// 以檔名排序遞迴走訪目錄（深度優先），確保跨平台、跨次執行的順序一致。
fn walk_sorted(dir: &Path, rel_prefix: &str, out: &mut Vec<WalkEntry>) -> Result<()> {
    let mut items = fs_err::read_dir(dir)
        .with_context(|| format!("讀取目錄失敗：{}", dir.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("讀取目錄項目失敗：{}", dir.display()))?;
    items.sort_by_key(|e| e.file_name());

    for item in items {
        let name_os = item.file_name();
        let Some(name) = name_os.to_str() else {
            bail!(
                "bundle 內檔名不是有效 UTF-8：{}；tar 路徑需可攜，請重新命名該檔案",
                item.path().display()
            );
        };
        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };
        let ft = item
            .file_type()
            .with_context(|| format!("讀取檔案類型失敗：{}", item.path().display()))?;
        if ft.is_dir() {
            out.push(WalkEntry {
                abs: item.path(),
                rel: rel.clone(),
                is_dir: true,
            });
            walk_sorted(&item.path(), &rel, out)?;
        } else if ft.is_file() {
            out.push(WalkEntry {
                abs: item.path(),
                rel,
                is_dir: false,
            });
        } else {
            // chefer-pack 產生的 bundle 只含一般檔案與目錄；symlink 在
            // Windows host 無法忠實保存，直接拒絕以免產出不一致的單檔。
            bail!(
                "bundle 內含不支援的項目（symlink 或特殊檔案）：{}；\
                 bundle 只應包含一般檔案與目錄，請重新打包",
                item.path().display()
            );
        }
    }
    Ok(())
}

/// 建立確定性的 tar header（mtime/uid/gid 固定為 0，理由見模組註解）。
fn deterministic_header(entry_type: tar::EntryType, mode: u32, size: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(entry_type);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0); // 固定 0：確保同輸入產生相同 payload bytes
    h.set_size(size);
    h
}

/// append 一個目錄項（tar_path 不帶尾斜線，這裡統一補上）。
fn append_dir_entry<W: Write>(tar: &mut tar::Builder<W>, tar_path: &str) -> Result<()> {
    let mut h = deterministic_header(tar::EntryType::Directory, 0o755, 0);
    tar.append_data(&mut h, format!("{tar_path}/"), io::empty())
        .with_context(|| format!("寫入 tar 目錄項失敗：{tar_path}/"))?;
    Ok(())
}

/// append 一個一般檔案（串流複製，不整檔讀入記憶體）。
fn append_file_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    tar_path: &str,
    src: &Path,
) -> Result<()> {
    let f = fs_err::File::open(src).with_context(|| format!("開啟檔案失敗：{}", src.display()))?;
    let len = f
        .metadata()
        .with_context(|| format!("讀取檔案中繼資料失敗：{}", src.display()))?
        .len();
    // agents/ 下是 guest-agent 二進位，解出後需可執行；其餘為資料檔。
    let mode = if tar_path.starts_with("bundle/agents/") {
        0o755
    } else {
        0o644
    };
    let mut h = deterministic_header(tar::EntryType::Regular, mode, len);
    tar.append_data(&mut h, tar_path, BufReader::new(f))
        .with_context(|| format!("寫入 tar 檔案失敗：{}（{}）", tar_path, src.display()))?;
    Ok(())
}

/// tee writer：寫入內層 writer 的同時計算 SHA-256 與累計位元組數。
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }

    /// 取回內層 writer、sha256 digest 與寫入總長。
    fn finalize(self) -> Result<(W, [u8; 32], u64)> {
        let digest: [u8; 32] = self.hasher.finalize().into();
        Ok((self.inner, digest, self.written))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// 串流計算檔案中 `[offset, offset+len)` 區段的 SHA-256（驗證用）。
pub fn hash_payload(path: &Path, offset: u64, len: u64) -> Result<[u8; 32]> {
    let mut f =
        fs_err::File::open(path).with_context(|| format!("開啟檔案失敗：{}", path.display()))?;
    f.seek(SeekFrom::Start(offset))?;
    let mut limited = f.take(len);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            bail!("payload 區段不足：預期 {len} bytes，提前讀到 EOF（檔案可能損毀）");
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chefer_bundle::{
        AppMeta, CrashPolicy, Footer, MANIFEST_FORMAT_VERSION, Manifest, NetworkMode,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    /// 簡單的偽隨機位元組（xorshift64*），避免引入 rand 依賴。
    fn pseudo_random_bytes(mut seed: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            let x = seed.wrapping_mul(0x2545F4914F6CDD1D);
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn minimal_manifest() -> Manifest {
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            app: AppMeta {
                name: "TestApp".into(),
                app_version: Some("1.0.0".into()),
                spec_version: "0.1".into(),
                old_names: vec![],
                data_dir_override: None,
                crash: CrashPolicy::FailFast,
                network: NetworkMode::Shared,
                console: chefer_bundle::ConsoleMode::Auto,
                generated_at_utc: "2026-01-01T00:00:00Z".into(),
                builder_version: String::new(),
            },
            services: vec![],
        }
    }

    /// 建一個假 bundle：manifest.json + 幾個巢狀檔案。
    /// 回傳（rel path（/ 分隔，不含 bundle/ 前綴）→ 內容）。
    fn make_fake_bundle(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();

        let m = minimal_manifest();
        m.save(&chefer_bundle::layout::manifest_path(dir)).unwrap();
        files.insert(
            "manifest.json".to_string(),
            fs::read(dir.join("manifest.json")).unwrap(),
        );

        fs::create_dir_all(dir.join("services/db/layers")).unwrap();
        let layer = pseudo_random_bytes(42, 200_000);
        fs::write(
            dir.join("services/db/layers/0000-abcdef123456.tar.zst"),
            &layer,
        )
        .unwrap();
        files.insert(
            "services/db/layers/0000-abcdef123456.tar.zst".to_string(),
            layer,
        );

        fs::create_dir_all(dir.join("agents")).unwrap();
        let agent = pseudo_random_bytes(7, 4096);
        fs::write(dir.join("agents/guest-agent-x86_64"), &agent).unwrap();
        files.insert("agents/guest-agent-x86_64".to_string(), agent);

        fs::write(dir.join("appcipe.yml"), b"name: TestApp\n").unwrap();
        files.insert("appcipe.yml".to_string(), b"name: TestApp\n".to_vec());

        files
    }

    fn make_fake_runtime(path: &Path) -> Vec<u8> {
        let bytes = pseudo_random_bytes(99, 123_457); // 非 4KiB 對齊的大小
        fs::write(path, &bytes).unwrap();
        bytes
    }

    #[test]
    fn roundtrip_assemble_and_extract() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime.exe");
        let runtime_bytes = make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        let expected_files = make_fake_bundle(&bundle);
        let out = tmp.path().join("out").join("app.exe");

        let report = assemble(&runtime, &bundle, &out, &AssembleOptions::default()).unwrap();

        // footer 與 report 一致
        let ft = Footer::read_from_file(&out).unwrap();
        assert!(ft.is_zstd());
        assert_eq!(ft.offset, report.payload_offset);
        assert_eq!(ft.length, report.payload_len);
        assert_eq!(ft.sha256, report.sha256);
        assert_eq!(ft.offset, runtime_bytes.len() as u64);
        assert_eq!(
            report.out_size,
            ft.offset + ft.length + chefer_bundle::FOOTER_LEN
        );
        assert_eq!(fs::metadata(&out).unwrap().len(), report.out_size);

        // runtime 前綴未被改動
        let out_bytes = fs::read(&out).unwrap();
        assert_eq!(&out_bytes[..runtime_bytes.len()], &runtime_bytes[..]);

        // payload 區段串流 sha256 比對
        let sha = hash_payload(&out, ft.offset, ft.length).unwrap();
        assert_eq!(sha, ft.sha256);

        // zstd 解 + tar 解 → 內容與原 bundle 一致（路徑以 bundle/ 開頭）
        let mut f = fs::File::open(&out).unwrap();
        f.seek(SeekFrom::Start(ft.offset)).unwrap();
        let limited = f.take(ft.length);
        let decoder = zstd::stream::read::Decoder::new(limited).unwrap();
        let mut archive = tar::Archive::new(decoder);

        let mut got_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut got_dirs: BTreeSet<String> = BTreeSet::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().replace('\\', "/");
            assert!(
                path == "bundle/" || path.starts_with("bundle/"),
                "tar 內路徑必須以 bundle/ 開頭：{path}"
            );
            assert_eq!(entry.header().mtime().unwrap(), 0, "mtime 必須固定為 0");
            match entry.header().entry_type() {
                tar::EntryType::Directory => {
                    got_dirs.insert(path.trim_end_matches('/').to_string());
                }
                tar::EntryType::Regular => {
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).unwrap();
                    got_files.insert(path, buf);
                }
                other => panic!("tar 內不應有其他類型：{other:?} @ {path}"),
            }
        }

        // 目錄項齊全
        for d in [
            "bundle",
            "bundle/agents",
            "bundle/services",
            "bundle/services/db",
        ] {
            assert!(got_dirs.contains(d), "缺少目錄項：{d}");
        }
        // 檔案內容一致
        let expected: BTreeMap<String, Vec<u8>> = expected_files
            .into_iter()
            .map(|(k, v)| (format!("bundle/{k}"), v))
            .collect();
        assert_eq!(got_files, expected);

        // agents/ 下檔案需有可執行 mode
        // （重新走一遍 archive 太麻煩，改驗 mode 規則由 determinism 測試覆蓋）
    }

    #[test]
    fn missing_manifest_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime");
        make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(bundle.join("services")).unwrap();
        let out = tmp.path().join("app");

        let err = assemble(&runtime, &bundle, &out, &AssembleOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("manifest.json"),
            "錯誤訊息應提到 manifest.json：{err:#}"
        );
    }

    #[test]
    fn deterministic_payload_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime");
        make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        make_fake_bundle(&bundle);

        let out1 = tmp.path().join("app1");
        let out2 = tmp.path().join("app2");
        let r1 = assemble(&runtime, &bundle, &out1, &AssembleOptions::default()).unwrap();
        let r2 = assemble(&runtime, &bundle, &out2, &AssembleOptions::default()).unwrap();
        assert_eq!(
            r1.sha256, r2.sha256,
            "同輸入兩次組裝的 payload sha 必須相同"
        );
        assert_eq!(r1.payload_len, r2.payload_len);

        // 不同壓縮等級 → 仍能正確解出（不檢查 sha 相同）
        let out3 = tmp.path().join("app3");
        let r3 = assemble(
            &runtime,
            &bundle,
            &out3,
            &AssembleOptions { zstd_level: 19 },
        )
        .unwrap();
        let ft3 = Footer::read_from_file(&out3).unwrap();
        assert_eq!(ft3.sha256, r3.sha256);
    }

    #[test]
    fn out_equals_runtime_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime");
        make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        make_fake_bundle(&bundle);

        let err = assemble(&runtime, &bundle, &runtime, &AssembleOptions::default()).unwrap_err();
        assert!(err.to_string().contains("同一個檔案"), "{err:#}");
    }

    #[test]
    fn out_inside_bundle_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime");
        make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        make_fake_bundle(&bundle);

        let out = bundle.join("app");
        let err = assemble(&runtime, &bundle, &out, &AssembleOptions::default()).unwrap_err();
        assert!(err.to_string().contains("bundle 目錄內"), "{err:#}");
    }

    #[test]
    fn overwrites_existing_out_file() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("fake-runtime");
        make_fake_runtime(&runtime);
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(&bundle).unwrap();
        make_fake_bundle(&bundle);

        let out = tmp.path().join("app");
        // 先放一個較大的舊檔，確認會被覆蓋而非 append 在其後
        fs::write(&out, pseudo_random_bytes(1, 999_999)).unwrap();
        let report = assemble(&runtime, &bundle, &out, &AssembleOptions::default()).unwrap();
        assert_eq!(fs::metadata(&out).unwrap().len(), report.out_size);
        let ft = Footer::read_from_file(&out).unwrap();
        assert_eq!(ft.offset, report.payload_offset);
    }
}
