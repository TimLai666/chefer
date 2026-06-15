//! rootfs 組裝：依序解開各 layer（zstd tar）、套用 whiteout、寫入快取。
//!
//! 跨平台部分（chain hash、快取路徑、路徑安全檢查）放在本模組頂層以便在
//! Windows 上測試；實際檔案操作（解 tar、symlink、權限）僅在 Linux 編譯。

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

/// rootfs 快取完成標記檔名（位於 rootfs 目錄內）。
pub const COMPLETE_MARKER: &str = ".complete";

/// 預設 rootfs 快取根目錄：`<data_dir>/.rootfs-cache`。
pub fn default_cache_root(data_dir: &Path) -> PathBuf {
    data_dir.join(".rootfs-cache")
}

/// chain hash = sha256(所有 diff_id 以 `\n` 串接)，回傳 64 碼小寫 hex。
pub fn chain_hash<S: AsRef<str>>(diff_ids: &[S]) -> String {
    let joined = diff_ids
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    hex::encode(Sha256::digest(joined.as_bytes()))
}

/// rootfs 快取目錄名：`<svc>-<chain_hash 前 12 碼>`。
pub fn rootfs_dir_name<S: AsRef<str>>(svc: &str, diff_ids: &[S]) -> String {
    let hash = chain_hash(diff_ids);
    format!("{svc}-{}", &hash[..12])
}

/// 服務的 rootfs 快取目錄完整路徑。
pub fn service_rootfs_dir(cache_root: &Path, svc: &chefer_bundle::ServiceEntry) -> PathBuf {
    let diff_ids: Vec<&str> = svc.layers.iter().map(|l| l.diff_id.as_str()).collect();
    cache_root.join(rootfs_dir_name(&svc.name, &diff_ids))
}

/// 快取是否已完成（目錄存在且含 `.complete` 標記）。
pub fn is_rootfs_complete(dir: &Path) -> bool {
    dir.is_dir() && dir.join(COMPLETE_MARKER).is_file()
}

/// tar entry 路徑（rootfs 內容）的安全檢查與正規化。
///
/// 此處處理的是「容器 rootfs 內」的 Linux 檔案，落地到 Linux 檔案系統：
/// - 拒絕：絕對路徑（以 `/` 開頭）、含 `..` 的成分（路徑穿越）。
/// - **允許** `:` 與 `\`：在 Linux 上兩者都是合法檔名字元，且 Debian/Ubuntu 的
///   dpkg multiarch 中繼資料大量使用 `:`（如 `var/lib/dpkg/info/gcc-12-base:arm64.list`）。
///   （tar 規格僅以 `/` 為路徑分隔，故 `\` 必為字面檔名，不得當成分隔。）
/// - `Ok(None)` 表示路徑正規化後為空（如 `"./"`），呼叫端應跳過該 entry。
pub fn sanitize_rel_path(entry_path: &str) -> Result<Option<PathBuf>> {
    if entry_path.starts_with('/') {
        bail!("tar entry uses an absolute path, rejected: {entry_path}");
    }
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for comp in entry_path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            bail!("tar entry contains a parent-directory reference (..), rejected: {entry_path}");
        }
        out.push(comp);
        depth += 1;
    }
    if depth == 0 {
        return Ok(None);
    }
    Ok(Some(out))
}

#[cfg(target_os = "linux")]
pub use linux::{
    RootfsLease, assemble_overlay_layers, assemble_rootfs_at, assemble_service_rootfs,
    overlay_supported,
};

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    use anyhow::{Context, Result, bail};

    use super::{COMPLETE_MARKER, is_rootfs_complete, sanitize_rel_path, service_rootfs_dir};
    use crate::whiteout::{WhiteoutAction, parse_whiteout};

    /// 已組裝完成的 rootfs 租約：在 run 期間持有共享 flock，
    /// 阻止其他並行 instance 在我們使用中刪除這個快取目錄。
    ///
    /// 背景（對抗式審查發現）：同一 kit 打包的多個 app（或同一 app 開兩次）
    /// 共用同名 distro 與同一 `<cache>/<svc>-<chain12>` 目錄。若無鎖，
    /// 後啟動者結束時的 `remove_dir_all` 會刪掉前者正在 pivot_root 使用的 rootfs，
    /// 造成運行中服務的根檔案系統被破壞。並行組裝也會互相刪除半成品。
    ///
    /// 解法：組裝期持 `LOCK_EX`（序列化組裝/重建），完成後降為 `LOCK_SH`
    /// 並持有至 run 結束；清理時嘗試非阻塞 `LOCK_EX`，唯有無其他共享持有者
    /// （即沒有別的 instance 在用）時才真正刪除。
    pub struct RootfsLease {
        dir: PathBuf,
        /// 持有 flock 的鎖檔（drop 時自動釋放鎖）。
        lock: fs::File,
    }

    impl RootfsLease {
        /// rootfs 目錄路徑。
        pub fn path(&self) -> &Path {
            &self.dir
        }

        /// 嘗試在「無其他 instance 使用」時刪除此 rootfs 快取。
        /// 回傳是否真的刪除（false = 仍有其他使用者，已安全跳過）。
        pub fn cleanup(self) -> bool {
            // 升級為非阻塞 EX：成功代表此 fd 上不再與任何「其他開檔描述」的鎖衝突，
            // 亦即沒有別的 instance 持有 SH → 可安全刪除。
            if flock(&self.lock, libc::LOCK_EX | libc::LOCK_NB).is_ok() {
                let _ = fs::remove_dir_all(&self.dir);
                true
            } else {
                false
            }
        }
    }

    /// 對 flock 鎖檔套用 flock(2)。
    fn flock(file: &fs::File, op: libc::c_int) -> std::io::Result<()> {
        // SAFETY: 傳入有效的 fd 與合法的 flock 操作常數。
        let r = unsafe { libc::flock(file.as_raw_fd(), op) };
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// 組裝（或重用快取的）服務 rootfs，回傳持有共享鎖的 [`RootfsLease`]。
    ///
    /// 目標目錄：`<cache_root>/<svc>-<chain_hash12>`；已存在且含 `.complete` → 直接重用。
    /// 存在但無 `.complete`（上次中斷）→ 整個移除重建。
    ///
    /// 鎖策略（先共享、需要時才升級獨佔的雙重檢查）：
    /// 1. 先取 `LOCK_SH`，若快取已完成 → 直接以共享鎖重用（多個 instance 可並行，
    ///    彼此不阻塞——這是重點：絕不能在「重用快取」的常見路徑上互相等待）。
    /// 2. 未完成 → 釋放 SH、取 `LOCK_EX` 重建，重建後**再次檢查**（避免與其他
    ///    process 重複建置），最後降回 `LOCK_SH` 持有至 run 結束。
    ///
    /// 持有的 SH 鎖會擋下其他 instance 的 cleanup（需 EX），確保使用中的 rootfs
    /// 不被刪除。
    pub fn assemble_service_rootfs(
        bundle_dir: &Path,
        svc: &chefer_bundle::ServiceEntry,
        cache_root: &Path,
    ) -> Result<RootfsLease> {
        let target = service_rootfs_dir(cache_root, svc);
        fs::create_dir_all(cache_root).with_context(|| {
            format!(
                "failed to create cache root directory: {}",
                cache_root.display()
            )
        })?;

        // 鎖檔放在 cache_root（不在 target 內，因為 target 會被 remove_dir_all）。
        let dir_name = target
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rootfs".into());
        let lock_path = cache_root.join(format!(".{dir_name}.lock"));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open rootfs lock file: {}", lock_path.display()))?;

        // 1) 先取共享鎖：快取已完成即走快路徑（並行 instance 互不阻塞）。
        flock(&lock, libc::LOCK_SH).with_context(|| {
            format!(
                "failed to acquire rootfs shared lock: {}",
                lock_path.display()
            )
        })?;
        if is_rootfs_complete(&target) {
            return Ok(RootfsLease { dir: target, lock });
        }

        // 2) 未完成 → 釋放共享鎖、取獨佔鎖重建（獨佔只在「真的要建」時才需要）。
        flock(&lock, libc::LOCK_UN).with_context(|| {
            format!(
                "failed to release rootfs shared lock: {}",
                lock_path.display()
            )
        })?;
        flock(&lock, libc::LOCK_EX).with_context(|| {
            format!(
                "failed to acquire rootfs exclusive lock: {}",
                lock_path.display()
            )
        })?;

        // 雙重檢查：等待獨佔鎖期間可能已被其他 process 建好。
        if !is_rootfs_complete(&target) {
            if target.exists() {
                fs::remove_dir_all(&target).with_context(|| {
                    format!(
                        "failed to remove incomplete rootfs cache: {}; delete it manually and retry",
                        target.display()
                    )
                })?;
            }
            fs::create_dir_all(&target).with_context(|| {
                format!("failed to create rootfs directory: {}", target.display())
            })?;

            if let Err(e) = assemble_rootfs_at(bundle_dir, svc, &target) {
                // 失敗時盡力清掉半成品，避免下次誤用
                let _ = fs::remove_dir_all(&target);
                return Err(e);
            }

            fs::write(target.join(COMPLETE_MARKER), b"").with_context(|| {
                format!(
                    "failed to write {COMPLETE_MARKER} marker: {}",
                    target.display()
                )
            })?;
        }

        // 3) 降回 SH 並持有至 run 結束（允許並行共用，擋下他人 cleanup）。
        flock(&lock, libc::LOCK_SH).with_context(|| {
            format!(
                "failed to downgrade to rootfs shared lock: {}",
                lock_path.display()
            )
        })?;

        Ok(RootfsLease { dir: target, lock })
    }

    /// 直接把服務的所有層解到 `out_dir`（不經快取；`assemble-rootfs` 除錯指令亦用此）。
    pub fn assemble_rootfs_at(
        bundle_dir: &Path,
        svc: &chefer_bundle::ServiceEntry,
        out_dir: &Path,
    ) -> Result<()> {
        fs::create_dir_all(out_dir)
            .with_context(|| format!("failed to create rootfs directory: {}", out_dir.display()))?;
        // 目錄權限延後套用：解壓中先確保 owner 可寫入（0o700），全部層解完後再 chmod 成精確值
        let mut dir_modes: BTreeMap<PathBuf, u32> = BTreeMap::new();

        // overlay 層必須**依序套用**（後層覆蓋前層 + whiteout 刪檔），但每層的 zstd 解壓
        // 互相獨立且 CPU 吃重（musl static 用純 Rust ruzstd，較慢）。故多層時以 worker pool
        // **提前平行解壓**到暫存 tar、主執行緒**仍照順序套用**——重疊解壓與套用以縮短首次組裝。
        let par = decode_parallelism(svc.layers.len());
        if par <= 1 {
            for layer in &svc.layers {
                let layer_path = bundle_dir.join(&layer.rel_path);
                let f = fs::File::open(&layer_path).with_context(|| {
                    format!(
                        "failed to open layer: {}; the bundle may be incomplete, please re-extract or repack",
                        layer_path.display()
                    )
                })?;
                let decoder = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(f))
                    .with_context(|| {
                    format!("zstd decompress failed: {}", layer_path.display())
                })?;
                apply_layer(out_dir, decoder, &mut dir_modes).with_context(|| {
                    format!("failed to extract layer: {}", layer_path.display())
                })?;
            }
        } else {
            apply_layers_pipelined(bundle_dir, &svc.layers, out_dir, &mut dir_modes, par)?;
        }

        // 套用精確的目錄權限（whiteout 可能已刪除部分目錄 → 忽略 NotFound）
        for (dir, mode) in &dir_modes {
            match fs::set_permissions(dir, fs::Permissions::from_mode(*mode & 0o7777)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("failed to set directory permissions: {}", dir.display())
                    });
                }
            }
        }
        Ok(())
    }

    /// 在 `root` 內安全解析相對路徑：逐成分前進，遇 symlink 以「容器內語意」解析
    /// （絕對目標視為相對 rootfs 根），保證結果不逃出 `root`。
    fn secure_resolve(root: &Path, rel: &Path) -> Result<PathBuf> {
        let mut cur = root.to_path_buf();
        // 待處理成分堆疊（反向存放，pop 取得下一個成分）
        let mut stack: Vec<std::ffi::OsString> = rel
            .components()
            .rev()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_os_string()),
                _ => None,
            })
            .collect();
        let mut budget = 256usize; // symlink 解析上限，防止循環

        while let Some(comp) = stack.pop() {
            let next = cur.join(&comp);
            match fs::symlink_metadata(&next) {
                Ok(md) if md.file_type().is_symlink() => {
                    budget = budget.checked_sub(1).filter(|&b| b > 0).ok_or_else(|| {
                        anyhow::anyhow!(
                            "symlink nesting too deep (possible cycle): {}",
                            next.display()
                        )
                    })?;
                    let target = fs::read_link(&next)
                        .with_context(|| format!("failed to read symlink: {}", next.display()))?;
                    if target.is_absolute() {
                        // 容器內絕對路徑 → 以 rootfs 根重新解析
                        cur = root.to_path_buf();
                    }
                    // 把 symlink 目標的成分推回堆疊（`..` 以「夾在 root 內」處理）
                    for c in target.components().rev() {
                        match c {
                            std::path::Component::Normal(s) => stack.push(s.to_os_string()),
                            std::path::Component::ParentDir => {
                                stack.push(std::ffi::OsString::from(".."))
                            }
                            _ => {}
                        }
                    }
                }
                _ => {
                    if comp == ".." {
                        // 不允許越過 rootfs 根
                        if cur != root {
                            cur.pop();
                        }
                    } else {
                        cur = next;
                    }
                }
            }
        }
        Ok(cur)
    }

    /// 多層平行解壓的執行緒數：單層 / 單核 → 1（走串流、免 temp）；否則取核心數，
    /// 上限 4（避免磁碟與記憶體壓力），且不超過層數。
    fn decode_parallelism(n_layers: usize) -> usize {
        if n_layers <= 1 {
            return 1;
        }
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        cpus.clamp(1, 4).min(n_layers)
    }

    /// 把單一 layer 的 zstd 解壓成未壓縮 tar，寫到 `out_tar`。
    fn decompress_one(layer_path: &Path, out_tar: &Path) -> Result<()> {
        let f = fs::File::open(layer_path).with_context(|| {
            format!(
                "failed to open layer: {}; the bundle may be incomplete, please re-extract or repack",
                layer_path.display()
            )
        })?;
        let mut dec = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(f))
            .with_context(|| format!("zstd decompress failed: {}", layer_path.display()))?;
        let mut out = fs::File::create(out_tar)
            .with_context(|| format!("failed to create staged layer tar: {}", out_tar.display()))?;
        std::io::copy(&mut dec, &mut out)
            .with_context(|| format!("zstd decompress failed: {}", layer_path.display()))?;
        Ok(()) // out 於此 drop（fs::File 無緩衝，drop 即關閉、內容已落地）
    }

    /// 平行（提前）解壓各層到暫存 tar，主執行緒**依序**套用。
    /// staging 目錄與 `out_dir` 同層（不放進 rootfs 內），結束時清除。
    fn apply_layers_pipelined(
        bundle_dir: &Path,
        layers: &[chefer_bundle::LayerRef],
        out_dir: &Path,
        dir_modes: &mut BTreeMap<PathBuf, u32>,
        workers: usize,
    ) -> Result<()> {
        let parent = out_dir.parent().unwrap_or_else(|| Path::new("."));
        let dirname = out_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rootfs".into());
        // 獨佔鎖保證同一 target 只有一個 builder；pid + 目錄名即足以唯一。
        let staging = parent.join(format!(".staging-{}-{dirname}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).with_context(|| {
            format!("failed to create layer staging dir: {}", staging.display())
        })?;

        let result = pipeline_inner(bundle_dir, layers, out_dir, dir_modes, workers, &staging);
        let _ = fs::remove_dir_all(&staging);
        result
    }

    /// 待套用層的狀態（主執行緒只會依序消費 index 0..n 各一次）。
    enum Slot {
        Pending,
        Ready(PathBuf),
        Failed(String),
        Taken,
    }
    struct PipeState {
        slots: Vec<Slot>,
        applied: usize,
        stop: bool, // 完成或出錯 → 通知 worker 收工
    }

    fn pipeline_inner(
        bundle_dir: &Path,
        layers: &[chefer_bundle::LayerRef],
        out_dir: &Path,
        dir_modes: &mut BTreeMap<PathBuf, u32>,
        workers: usize,
        staging: &Path,
    ) -> Result<()> {
        let n = layers.len();
        let limit = workers + 1; // 提前解壓的 look-ahead 視窗（限制暫存 tar 數量）
        let st = Mutex::new(PipeState {
            slots: (0..n).map(|_| Slot::Pending).collect(),
            applied: 0,
            stop: false,
        });
        let cv = Condvar::new();
        let next = AtomicUsize::new(0);

        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= n {
                            break;
                        }
                        // 背壓：i 必須落在 [applied, applied+limit) 視窗內才解壓；stop 則收工。
                        {
                            let mut g = st.lock().unwrap();
                            while !g.stop && i >= g.applied + limit {
                                g = cv.wait(g).unwrap();
                            }
                            if g.stop {
                                break;
                            }
                        }
                        let tar_path = staging.join(format!("{i}.tar"));
                        let res = decompress_one(&bundle_dir.join(&layers[i].rel_path), &tar_path);
                        let mut g = st.lock().unwrap();
                        match res {
                            Ok(()) => g.slots[i] = Slot::Ready(tar_path),
                            Err(e) => {
                                g.slots[i] = Slot::Failed(format!("{e:#}"));
                                g.stop = true;
                            }
                        }
                        cv.notify_all();
                    }
                });
            }

            // 主執行緒：依序等待第 i 層就緒 → 套用 → 推進視窗。
            let mut run = || -> Result<()> {
                for i in 0..n {
                    let tar_path = {
                        let mut g = st.lock().unwrap();
                        loop {
                            match std::mem::replace(&mut g.slots[i], Slot::Taken) {
                                Slot::Ready(p) => break p,
                                Slot::Failed(e) => return Err(anyhow::anyhow!(e)),
                                other => {
                                    g.slots[i] = other;
                                    g = cv.wait(g).unwrap();
                                }
                            }
                        }
                    };
                    let f = fs::File::open(&tar_path).with_context(|| {
                        format!("failed to open staged layer tar: {}", tar_path.display())
                    })?;
                    apply_layer(out_dir, std::io::BufReader::new(f), dir_modes)
                        .with_context(|| format!("failed to extract layer {i}"))?;
                    let _ = fs::remove_file(&tar_path);
                    let mut g = st.lock().unwrap();
                    g.applied = i + 1;
                    cv.notify_all();
                }
                Ok(())
            };
            let result = run();
            // 不論成敗都通知 worker 收工（避免成功時仍多解壓、或出錯時 worker 卡在等待）。
            {
                let mut g = st.lock().unwrap();
                g.stop = true;
                cv.notify_all();
            }
            result
        })
    }

    // ─────────────────────── overlayfs / lazy rootfs（僅 root 後端）───────────────────────
    //
    // root 後端（WSL2 / macOS VM / native-root）可改用 overlayfs：各層解到「以 diff_id 為名」
    // 的獨立唯讀 lowerdir（跨服務/跨 image 共用、去重、持久），exec 時掛 overlay（lower=各層、
    // upper/work=每次執行的暫存可寫層）再 pivot_root——免合併複製、掛載瞬間完成。
    // rootless 無法 mknod whiteout 裝置、unprivileged overlay 受限 → 維持現有合併路徑。
    //
    // OCI whiteout → overlay whiteout 轉換（解層時）：
    // - `.wh.<name>`        → 在該層 dir 建字元裝置 0:0（overlay 的 whiteout 表示法）。
    // - `.wh..wh..opq`      → 對該目錄設 `trusted.overlay.opaque=y`。
    // 兩者皆需真 root（mknod 裝置 / trusted.* xattr），故此路徑僅在 root 後端啟用。

    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// 是否可用 overlay：必須是真 root（rootless 不能 mknod whiteout / 掛 unpriv overlay 受限），
    /// kernel 支援 overlay，且實測掛載成功。
    pub fn overlay_supported() -> bool {
        if !nix::unistd::geteuid().is_root() {
            return false;
        }
        match fs::read_to_string("/proc/filesystems") {
            Ok(s)
                if s.lines()
                    .any(|l| l.split('\t').next_back() == Some("overlay")) => {}
            _ => return false,
        }
        overlay_mount_selftest()
    }

    /// 在新 mount ns 內試掛一個最小 overlay（兩個空 lowerdir + upper + work），驗證真的能掛。
    /// 以子行程隔離（unshare(NEWNS) + mount 不污染本行程）；成功 → 退 0。
    fn overlay_mount_selftest() -> bool {
        let base = std::env::temp_dir().join(format!("chefer-ovl-selftest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in ["l1", "l2", "u", "w", "m"] {
            if fs::create_dir_all(base.join(d)).is_err() {
                return false;
            }
        }
        // SAFETY: fork 後子行程僅呼叫 async-signal 安全的系統呼叫並 _exit。
        let ok = match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { child }) => {
                matches!(
                    nix::sys::wait::waitpid(child, None),
                    Ok(nix::sys::wait::WaitStatus::Exited(_, 0))
                )
            }
            Ok(nix::unistd::ForkResult::Child) => {
                use nix::mount::{MsFlags, mount};
                use nix::sched::{CloneFlags, unshare};
                let code = (|| -> i32 {
                    if unshare(CloneFlags::CLONE_NEWNS).is_err() {
                        return 1;
                    }
                    let opts = format!(
                        "lowerdir={}:{},upperdir={},workdir={}",
                        base.join("l1").display(),
                        base.join("l2").display(),
                        base.join("u").display(),
                        base.join("w").display(),
                    );
                    let r = mount(
                        Some("overlay"),
                        &base.join("m"),
                        Some("overlay"),
                        MsFlags::empty(),
                        Some(opts.as_str()),
                    );
                    if r.is_err() { 1 } else { 0 }
                })();
                unsafe { libc::_exit(code) };
            }
            Err(_) => false,
        };
        let _ = fs::remove_dir_all(&base);
        ok
    }

    /// 確保各層已解到獨立 lowerdir（以 diff_id 為名、內容定址、可共用），回傳 base→top 順序的目錄清單。
    pub fn assemble_overlay_layers(
        bundle_dir: &Path,
        svc: &chefer_bundle::ServiceEntry,
        cache_root: &Path,
    ) -> Result<Vec<PathBuf>> {
        let layers_root = cache_root.join("layers");
        fs::create_dir_all(&layers_root).with_context(|| {
            format!(
                "failed to create overlay layers dir: {}",
                layers_root.display()
            )
        })?;
        let mut dirs = Vec::with_capacity(svc.layers.len());
        for layer in &svc.layers {
            dirs.push(ensure_overlay_layer(bundle_dir, layer, &layers_root)?);
        }
        Ok(dirs)
    }

    /// 確保單層已解到 `<layers_root>/<diff_id>`（含 `.complete`）；內容定址 → 同層只解一次、跨 image 共用。
    fn ensure_overlay_layer(
        bundle_dir: &Path,
        layer: &chefer_bundle::LayerRef,
        layers_root: &Path,
    ) -> Result<PathBuf> {
        // diff_id 形如 "sha256:<hex>"；取 hex 當目錄名（檔名安全）。
        let key = layer.diff_id.rsplit(':').next().unwrap_or(&layer.diff_id);
        let dir = layers_root.join(key);

        // 以鎖檔序列化建置（共用快路徑：已 .complete 直接重用）。
        let lock_path = layers_root.join(format!(".{key}.lock"));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open layer lock: {}", lock_path.display()))?;
        flock(&lock, libc::LOCK_SH).context("failed to acquire layer shared lock")?;
        if is_rootfs_complete(&dir) {
            return Ok(dir);
        }
        flock(&lock, libc::LOCK_UN).ok();
        flock(&lock, libc::LOCK_EX).context("failed to acquire layer exclusive lock")?;

        if !is_rootfs_complete(&dir) {
            if dir.exists() {
                fs::remove_dir_all(&dir).with_context(|| {
                    format!("failed to remove incomplete layer dir: {}", dir.display())
                })?;
            }
            fs::create_dir_all(&dir)
                .with_context(|| format!("failed to create layer dir: {}", dir.display()))?;
            let layer_path = bundle_dir.join(&layer.rel_path);
            let f = fs::File::open(&layer_path).with_context(|| {
                format!(
                    "failed to open layer: {}; the bundle may be incomplete, please re-extract or repack",
                    layer_path.display()
                )
            })?;
            let dec = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(f))
                .with_context(|| format!("zstd decompress failed: {}", layer_path.display()))?;
            if let Err(e) = extract_layer_overlay(&dir, dec) {
                let _ = fs::remove_dir_all(&dir);
                return Err(e).with_context(|| {
                    format!("failed to extract overlay layer: {}", layer_path.display())
                });
            }
            fs::write(dir.join(COMPLETE_MARKER), b"")
                .with_context(|| format!("failed to write layer .complete: {}", dir.display()))?;
        }
        flock(&lock, libc::LOCK_SH).ok();
        Ok(dir)
    }

    /// 解單層到 overlay lowerdir：OCI whiteout 轉成 overlay whiteout，其餘 entry 正常落地（不刪除）。
    fn extract_layer_overlay<R: Read>(layer_dir: &Path, reader: R) -> Result<()> {
        let mut dir_modes: BTreeMap<PathBuf, u32> = BTreeMap::new();
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries().context("failed to read tar entry")? {
            let mut entry = entry.context("failed to read tar entry")?;
            let path_str = entry
                .path()
                .context("could not parse tar entry path")?
                .to_string_lossy()
                .into_owned();

            if let Some(action) = parse_whiteout(&path_str) {
                apply_overlay_whiteout(layer_dir, &action)?;
                continue;
            }
            let Some(rel) = sanitize_rel_path(&path_str)? else {
                continue;
            };
            let parent_abs = match rel.parent() {
                Some(p) if !p.as_os_str().is_empty() => secure_resolve(layer_dir, p)?,
                _ => layer_dir.to_path_buf(),
            };
            fs::create_dir_all(&parent_abs)
                .with_context(|| format!("failed to create directory: {}", parent_abs.display()))?;
            let file_name = rel
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("tar entry is missing a file name: {path_str}"))?;
            let dest = parent_abs.join(file_name);
            extract_entry(layer_dir, &mut entry, &dest, &path_str, &mut dir_modes)?;
        }
        for (dir, mode) in &dir_modes {
            match fs::set_permissions(dir, fs::Permissions::from_mode(*mode & 0o7777)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("failed to set directory permissions: {}", dir.display())
                    });
                }
            }
        }
        Ok(())
    }

    /// 在 overlay lowerdir 內表示 whiteout：刪除 → 字元裝置 0:0；opaque → trusted.overlay.opaque。
    fn apply_overlay_whiteout(layer_dir: &Path, action: &WhiteoutAction) -> Result<()> {
        match action {
            WhiteoutAction::Remove { path } => {
                let Some(rel) = sanitize_rel_path(path)? else {
                    return Ok(());
                };
                let parent_abs = match rel.parent() {
                    Some(p) if !p.as_os_str().is_empty() => secure_resolve(layer_dir, p)?,
                    _ => layer_dir.to_path_buf(),
                };
                fs::create_dir_all(&parent_abs)?;
                let target = parent_abs.join(rel.file_name().unwrap_or_default());
                let _ = remove_existing(&target);
                mknod_whiteout(&target)?;
            }
            WhiteoutAction::Opaque { dir } => {
                let target = match sanitize_rel_path(dir)? {
                    Some(rel) => secure_resolve(layer_dir, &rel)?,
                    None => layer_dir.to_path_buf(),
                };
                fs::create_dir_all(&target)?;
                set_overlay_opaque(&target)?;
            }
        }
        Ok(())
    }

    /// 建立 overlay whiteout 字元裝置（dev 0:0）。需真 root。
    fn mknod_whiteout(path: &Path) -> Result<()> {
        let c = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path contains NUL: {}", path.display()))?;
        // SAFETY: 有效路徑 + S_IFCHR + dev 0:0。
        let r = unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, libc::makedev(0, 0)) };
        if r != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "failed to create overlay whiteout device (needs real root): {}",
                    path.display()
                )
            });
        }
        Ok(())
    }

    /// 對目錄設 overlay 不透明標記（trusted.overlay.opaque=y）。需真 root。
    fn set_overlay_opaque(dir: &Path) -> Result<()> {
        let cp = CString::new(dir.as_os_str().as_bytes())
            .with_context(|| format!("path contains NUL: {}", dir.display()))?;
        let name = c"trusted.overlay.opaque";
        // SAFETY: 有效路徑/名稱/值。
        let r = unsafe {
            libc::setxattr(
                cp.as_ptr(),
                name.as_ptr(),
                b"y".as_ptr() as *const libc::c_void,
                1,
                0,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "failed to set trusted.overlay.opaque (needs real root): {}",
                    dir.display()
                )
            });
        }
        Ok(())
    }

    /// 解開單一層：whiteout 先處理，其他 entry 正常落地。
    fn apply_layer<R: Read>(
        root: &Path,
        reader: R,
        dir_modes: &mut BTreeMap<PathBuf, u32>,
    ) -> Result<()> {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries().context("failed to read tar entry")? {
            let mut entry = entry.context("failed to read tar entry")?;
            let path_buf = entry
                .path()
                .context("could not parse tar entry path")?
                .into_owned();
            // tar 規格僅以 `/` 為分隔，不可把 `\` 當分隔（`\` 是合法 Linux 檔名字元）。
            let path_str = path_buf.to_string_lossy().into_owned();

            // whiteout：不落地，只對既有內容動作
            if let Some(action) = parse_whiteout(&path_str) {
                apply_whiteout(root, &action)?;
                continue;
            }

            let Some(rel) = sanitize_rel_path(&path_str)? else {
                continue; // 空路徑（如 "./"）
            };

            // 解析父目錄（容器內語意的 symlink 追蹤，保證不逃出 root）
            let parent_abs = match rel.parent() {
                Some(p) if p.as_os_str().is_empty() => root.to_path_buf(),
                Some(p) => secure_resolve(root, p)?,
                None => root.to_path_buf(),
            };
            fs::create_dir_all(&parent_abs)
                .with_context(|| format!("failed to create directory: {}", parent_abs.display()))?;
            let file_name = rel
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("tar entry is missing a file name: {path_str}"))?;
            let dest = parent_abs.join(file_name);

            extract_entry(root, &mut entry, &dest, &path_str, dir_modes)?;
        }
        Ok(())
    }

    /// 套用 whiteout 動作。
    fn apply_whiteout(root: &Path, action: &WhiteoutAction) -> Result<()> {
        match action {
            WhiteoutAction::Opaque { dir } => {
                let Some(rel) = sanitize_rel_path(dir)? else {
                    // rootfs 根的 opaque：清空整個 root 既有內容
                    clear_dir_contents(root)?;
                    return Ok(());
                };
                let abs = secure_resolve(root, &rel)?;
                if abs.is_dir() {
                    clear_dir_contents(&abs)?;
                }
            }
            WhiteoutAction::Remove { path } => {
                let Some(rel) = sanitize_rel_path(path)? else {
                    return Ok(());
                };
                let parent_abs = match rel.parent() {
                    Some(p) if !p.as_os_str().is_empty() => secure_resolve(root, p)?,
                    _ => root.to_path_buf(),
                };
                let target = parent_abs.join(rel.file_name().unwrap_or_default());
                remove_path(&target)?;
            }
        }
        Ok(())
    }

    /// 清空目錄內容但保留目錄本身。
    fn clear_dir_contents(dir: &Path) -> Result<()> {
        for e in fs::read_dir(dir)
            .with_context(|| format!("failed to read directory: {}", dir.display()))?
        {
            let e = e?;
            remove_path(&e.path())?;
        }
        Ok(())
    }

    /// 刪除檔案或整棵目錄（不存在則靜默成功）。
    fn remove_path(p: &Path) -> Result<()> {
        match fs::symlink_metadata(p) {
            Ok(md) => {
                if md.is_dir() {
                    fs::remove_dir_all(p)
                        .with_context(|| format!("failed to delete directory: {}", p.display()))?;
                } else {
                    fs::remove_file(p)
                        .with_context(|| format!("failed to delete file: {}", p.display()))?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to check path: {}", p.display())),
        }
    }

    /// 若 `dest` 已存在（任何型別），先移除——上層 layer 覆蓋下層內容。
    fn remove_existing(dest: &Path) -> Result<()> {
        remove_path(dest)
    }

    /// 解開單一 entry（已通過路徑安全檢查）。
    fn extract_entry<R: Read>(
        root: &Path,
        entry: &mut tar::Entry<'_, R>,
        dest: &Path,
        path_str: &str,
        dir_modes: &mut BTreeMap<PathBuf, u32>,
    ) -> Result<()> {
        use tar::EntryType;

        let header = entry.header();
        let mode = header.mode().unwrap_or(0o644);
        let etype = header.entry_type();

        match etype {
            EntryType::Directory => {
                // 既有同名非目錄 → 移除；既有目錄 → 保留內容只更新權限
                match fs::symlink_metadata(dest) {
                    Ok(md) if !md.is_dir() => remove_existing(dest)?,
                    _ => {}
                }
                fs::create_dir_all(dest)
                    .with_context(|| format!("failed to create directory: {}", dest.display()))?;
                // 先給 owner rwx 確保後續可寫入；精確權限延後套用
                fs::set_permissions(dest, fs::Permissions::from_mode((mode & 0o7777) | 0o700))
                    .with_context(|| {
                        format!("failed to set directory permissions: {}", dest.display())
                    })?;
                dir_modes.insert(dest.to_path_buf(), mode);
            }
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                remove_existing(dest)?;
                let mut f = fs::File::create(dest)
                    .with_context(|| format!("failed to create file: {}", dest.display()))?;
                std::io::copy(entry, &mut f)
                    .with_context(|| format!("failed to write file: {}", dest.display()))?;
                f.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
                    .with_context(|| {
                        format!("failed to set file permissions: {}", dest.display())
                    })?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .context("failed to read symlink target")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("symlink entry is missing a target: {path_str}")
                    })?;
                // symlink 目標保留原樣（容器內語意；絕對路徑指向 rootfs 內）
                remove_existing(dest)?;
                std::os::unix::fs::symlink(&target, dest).with_context(|| {
                    format!(
                        "failed to create symlink: {} → {}",
                        dest.display(),
                        target.display()
                    )
                })?;
            }
            EntryType::Link => {
                // hardlink 目標必須在 rootfs 內：先做路徑安全檢查再解析
                let target = entry
                    .link_name()
                    .context("failed to read hardlink target")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("hardlink entry is missing a target: {path_str}")
                    })?;
                let target_str = target.to_string_lossy();
                let target_str = target_str.strip_prefix('/').unwrap_or(&target_str);
                let Some(target_rel) = sanitize_rel_path(target_str)? else {
                    bail!("invalid hardlink target: {path_str} → {target_str}");
                };
                let target_abs = secure_resolve(root, &target_rel)?;
                remove_existing(dest)?;
                fs::hard_link(&target_abs, dest).with_context(|| {
                    format!(
                        "failed to create hardlink: {} → {} (the target must already exist inside the rootfs)",
                        dest.display(),
                        target_abs.display()
                    )
                })?;
            }
            EntryType::Fifo => {
                remove_existing(dest)?;
                nix::unistd::mkfifo(
                    dest,
                    nix::sys::stat::Mode::from_bits_truncate(mode & 0o7777),
                )
                .with_context(|| format!("failed to create fifo: {}", dest.display()))?;
            }
            EntryType::Char | EntryType::Block => {
                // userns 內通常無法 mknod 實體裝置；容器所需基本裝置由執行期掛載提供 → 略過
            }
            _ => {
                // XHeader/GNULongName 等由 tar crate 內部處理；其他罕見型別略過
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_hash_is_sha256_of_newline_joined_diff_ids() {
        let ids = ["sha256:aaaa", "sha256:bbbb"];
        let expect = hex::encode(Sha256::digest(b"sha256:aaaa\nsha256:bbbb"));
        assert_eq!(chain_hash(&ids), expect);
        // 單一層
        let one = ["sha256:cccc"];
        assert_eq!(
            chain_hash(&one),
            hex::encode(Sha256::digest(b"sha256:cccc"))
        );
    }

    #[test]
    fn rootfs_dir_name_uses_first_12_hex() {
        let ids = ["sha256:aaaa"];
        let name = rootfs_dir_name("db", &ids);
        let hash = chain_hash(&ids);
        assert_eq!(name, format!("db-{}", &hash[..12]));
    }

    #[test]
    fn different_layers_give_different_names() {
        let a = rootfs_dir_name("svc", &["sha256:1111"]);
        let b = rootfs_dir_name("svc", &["sha256:2222"]);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_completeness_logic() {
        let base =
            std::env::temp_dir().join(format!("guest-agent-test-cache-{}", std::process::id()));
        let dir = base.join("svc-abcdef123456");
        let _ = std::fs::remove_dir_all(&base);

        // 不存在 → 未完成
        assert!(!is_rootfs_complete(&dir));
        // 存在但無 .complete → 未完成
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_rootfs_complete(&dir));
        // 寫入 .complete → 完成
        std::fs::write(dir.join(COMPLETE_MARKER), b"").unwrap();
        assert!(is_rootfs_complete(&dir));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn default_cache_root_is_under_data_dir() {
        let p = default_cache_root(Path::new("/data"));
        assert!(p.ends_with(".rootfs-cache"));
    }

    #[test]
    fn sanitize_accepts_normal_paths() {
        let p = sanitize_rel_path("usr/bin/sh").unwrap().unwrap();
        assert_eq!(p, PathBuf::from("usr").join("bin").join("sh"));
        // `./` 前綴與尾端 `/` 正規化
        let p = sanitize_rel_path("./etc/").unwrap().unwrap();
        assert_eq!(p, PathBuf::from("etc"));
    }

    #[test]
    fn sanitize_empty_yields_none() {
        assert!(sanitize_rel_path("./").unwrap().is_none());
        assert!(sanitize_rel_path("").unwrap().is_none());
        assert!(sanitize_rel_path(".").unwrap().is_none());
    }

    #[test]
    fn sanitize_rejects_unsafe_paths() {
        assert!(sanitize_rel_path("/etc/passwd").is_err(), "絕對路徑");
        assert!(sanitize_rel_path("a/../../b").is_err(), "上層參照");
        assert!(sanitize_rel_path("..").is_err(), "純上層參照");
    }

    /// `:` 與 `\` 在 Linux 是合法檔名字元，rootfs 層解壓必須接受
    /// （Debian/Ubuntu 的 dpkg multiarch 中繼資料大量使用 `:`）。
    #[test]
    fn sanitize_accepts_linux_colon_and_backslash() {
        // Debian multiarch：gcc-12-base:arm64.list
        let p = sanitize_rel_path("var/lib/dpkg/info/gcc-12-base:arm64.list")
            .unwrap()
            .unwrap();
        assert_eq!(
            p,
            PathBuf::from("var")
                .join("lib")
                .join("dpkg")
                .join("info")
                .join("gcc-12-base:arm64.list")
        );
        // `C:` 視為字面檔名（非磁碟前綴）——它只是 rootfs 內某個叫 `C:` 的目錄
        let p = sanitize_rel_path("C:/windows").unwrap().unwrap();
        assert_eq!(p, PathBuf::from("C:").join("windows"));
        // 反斜線為字面檔名字元，整段成分保留
        let p = sanitize_rel_path(r"a\b/c").unwrap().unwrap();
        assert_eq!(p, PathBuf::from(r"a\b").join("c"));
    }
}

/// Linux 專屬整合測試：合成 zstd 層 → 組裝 rootfs → 驗證 whiteout / symlink /
/// hardlink / 快取行為（CI ubuntu runner 執行）。
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use sha2::{Digest, Sha256};

    use super::*;

    /// 以 tar::Builder 在記憶體內建一個層。
    struct LayerBuilder {
        builder: tar::Builder<Vec<u8>>,
    }

    impl LayerBuilder {
        fn new() -> Self {
            Self {
                builder: tar::Builder::new(Vec::new()),
            }
        }
        fn dir(mut self, path: &str, mode: u32) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(mode);
            self.builder
                .append_data(&mut h, path, std::io::empty())
                .unwrap();
            self
        }
        fn file(mut self, path: &str, content: &[u8], mode: u32) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(content.len() as u64);
            h.set_mode(mode);
            self.builder.append_data(&mut h, path, content).unwrap();
            self
        }
        fn symlink(mut self, path: &str, target: &str) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            self.builder.append_link(&mut h, path, target).unwrap();
            self
        }
        fn hardlink(mut self, path: &str, target: &str) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o644);
            self.builder.append_link(&mut h, path, target).unwrap();
            self
        }
        /// 結束建構，回傳（未壓縮 tar bytes, diff_id）。
        fn finish(self) -> (Vec<u8>, String) {
            let data = self.builder.into_inner().unwrap();
            let diff_id = format!("sha256:{}", hex::encode(Sha256::digest(&data)));
            (data, diff_id)
        }
    }

    /// 把多個層寫進假 bundle，回傳 (bundle_dir, ServiceEntry)。
    fn make_bundle(
        base: &Path,
        svc_name: &str,
        layers: Vec<(Vec<u8>, String)>,
    ) -> chefer_bundle::ServiceEntry {
        let layers_dir = chefer_bundle::layout::service_layers_dir(base, svc_name);
        std::fs::create_dir_all(&layers_dir).unwrap();
        let mut refs = Vec::new();
        for (i, (tar_bytes, diff_id)) in layers.into_iter().enumerate() {
            let name = chefer_bundle::layout::layer_file_name(i, &diff_id);
            let zst = ruzstd::encoding::compress_to_vec(
                &tar_bytes[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            std::fs::write(layers_dir.join(&name), &zst).unwrap();
            refs.push(chefer_bundle::LayerRef {
                rel_path: format!("services/{svc_name}/layers/{name}"),
                diff_id,
                size: Some(zst.len() as u64),
            });
        }
        chefer_bundle::ServiceEntry {
            name: svc_name.into(),
            platform: "linux/amd64".into(),
            layers: refs,
            image_config: chefer_bundle::ImageConfig::default(),
            cmd_override: None,
            env: BTreeMap::new(),
            workdir_override: None,
            persist_path: None,
            ports: vec![],
            mounts: vec![],
            interface_mode: chefer_bundle::InterfaceMode::None,
            depends_on: vec![],
            healthcheck: None,
        }
    }

    #[test]
    fn assembles_layers_with_whiteouts() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache = tmp.path().join("cache");

        // 層 0：基本檔案、權限、symlink、hardlink
        let l0 = LayerBuilder::new()
            .dir("etc", 0o755)
            .file("etc/a.txt", b"v1", 0o644)
            .dir("data", 0o755)
            .file("data/x", b"x", 0o644)
            .file("data/y", b"y", 0o644)
            .dir("bin", 0o755)
            .file("bin/prog", b"#!/bin/sh\n", 0o755)
            .symlink("etc/link", "a.txt")
            .hardlink("etc/hard", "etc/a.txt")
            .finish();
        // 層 1：whiteout 刪 etc/a.txt、opaque 清空 data、再新增 data/z、覆寫 bin/prog
        let l1 = LayerBuilder::new()
            .file("etc/.wh.a.txt", b"", 0o644)
            .file("data/.wh..wh..opq", b"", 0o644)
            .file("data/z", b"z", 0o600)
            .file("bin/prog", b"#!/bin/sh\necho v2\n", 0o700)
            .finish();

        let svc = make_bundle(&bundle, "app", vec![l0, l1]);
        let lease = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();
        let rootfs = lease.path();

        // whiteout：etc/a.txt 應被刪除；whiteout entry 本身不落地
        assert!(!rootfs.join("etc/a.txt").exists());
        assert!(!rootfs.join("etc/.wh.a.txt").exists());
        // opaque：data 只剩上層的 z
        assert!(rootfs.join("data").is_dir());
        assert!(!rootfs.join("data/x").exists());
        assert!(!rootfs.join("data/y").exists());
        assert_eq!(std::fs::read(rootfs.join("data/z")).unwrap(), b"z");
        assert!(!rootfs.join("data/.wh..wh..opq").exists());
        // 上層覆寫 + 權限保留
        assert_eq!(
            std::fs::read(rootfs.join("bin/prog")).unwrap(),
            b"#!/bin/sh\necho v2\n"
        );
        let mode = std::fs::metadata(rootfs.join("bin/prog"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o700);
        // symlink 原樣保留（hardlink 與來源同 inode；來源已被 whiteout 刪除但 hard 仍在）
        let link = std::fs::read_link(rootfs.join("etc/link")).unwrap();
        assert_eq!(link, std::path::PathBuf::from("a.txt"));
        assert_eq!(std::fs::read(rootfs.join("etc/hard")).unwrap(), b"v1");
        // 完成標記
        assert!(is_rootfs_complete(rootfs));
    }

    #[test]
    fn reuses_complete_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache = tmp.path().join("cache");
        let l0 = LayerBuilder::new().file("hello", b"hi", 0o644).finish();
        let svc = make_bundle(&bundle, "app", vec![l0]);

        let lease = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();
        let rootfs = lease.path().to_path_buf();
        // 在快取內放哨兵檔；重用時不應重建
        std::fs::write(rootfs.join("sentinel"), b"keep").unwrap();
        drop(lease); // 釋放鎖，模擬另一次啟動
        let lease2 = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();
        assert_eq!(rootfs, lease2.path());
        assert!(
            lease2.path().join("sentinel").exists(),
            "快取命中時不應重建 rootfs"
        );
    }

    #[test]
    fn rebuilds_incomplete_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache = tmp.path().join("cache");
        let l0 = LayerBuilder::new().file("hello", b"hi", 0o644).finish();
        let svc = make_bundle(&bundle, "app", vec![l0]);

        // 模擬中斷：目錄存在但沒有 .complete
        let target = service_rootfs_dir(&cache, &svc);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale"), b"old").unwrap();

        let lease = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();
        let rootfs = lease.path();
        assert!(!rootfs.join("stale").exists(), "不完整快取應整個重建");
        assert!(rootfs.join("hello").exists());
        assert!(is_rootfs_complete(rootfs));
    }

    #[test]
    fn cleanup_skips_while_other_lease_holds_shared_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache = tmp.path().join("cache");
        let l0 = LayerBuilder::new().file("hello", b"hi", 0o644).finish();
        let svc = make_bundle(&bundle, "app", vec![l0]);

        // instance A 持有租約（共享鎖）
        let lease_a = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();
        let dir = lease_a.path().to_path_buf();
        // instance B 重用快取、取得自己的共享鎖
        let lease_b = assemble_service_rootfs(&bundle, &svc, &cache).unwrap();

        // B 先結束並嘗試清理：A 仍持共享鎖 → 必須跳過刪除
        assert!(!lease_b.cleanup(), "仍有 A 在用時 B 不應刪除 rootfs");
        assert!(dir.is_dir(), "A 正在使用的 rootfs 不可被刪");

        // A 結束清理：此時無人使用 → 真正刪除
        assert!(lease_a.cleanup(), "最後使用者應能清除 rootfs");
        assert!(!dir.exists(), "無人使用後 rootfs 應被刪除");
    }

    #[test]
    fn absolute_symlink_resolves_inside_rootfs() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let out = tmp.path().join("rootfs");

        // 層 0：絕對 symlink dir → /real；層 1：透過 symlink 寫入 dir/inner.txt
        let l0 = LayerBuilder::new()
            .dir("real", 0o755)
            .symlink("dir", "/real")
            .finish();
        let l1 = LayerBuilder::new()
            .file("dir/inner.txt", b"inside", 0o644)
            .finish();
        let svc = make_bundle(&bundle, "app", vec![l0, l1]);

        assemble_rootfs_at(&bundle, &svc, &out).unwrap();
        // 容器內語意：/real 解析到 rootfs/real，不會逃到 host 的 /real
        assert_eq!(
            std::fs::read(out.join("real/inner.txt")).unwrap(),
            b"inside"
        );
        assert!(!Path::new("/real/inner.txt").exists());
    }

    #[test]
    fn rejects_escaping_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let out = tmp.path().join("rootfs");

        // hardlink 目標含 `..` → 拒絕
        let l0 = LayerBuilder::new()
            .file("a", b"x", 0o644)
            .hardlink("b", "../outside")
            .finish();
        let svc = make_bundle(&bundle, "app", vec![l0]);
        let err = assemble_rootfs_at(&bundle, &svc, &out).unwrap_err();
        assert!(
            format!("{err:#}").contains(".."),
            "錯誤應指出 .. 參照：{err:#}"
        );
    }

    #[test]
    fn symlink_pointing_outside_is_clamped() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let out = tmp.path().join("rootfs");

        // symlink up → ../../..（指向 rootfs 外）；上層透過它寫檔 → 應被夾在 rootfs 內
        let l0 = LayerBuilder::new().symlink("up", "../../..").finish();
        let l1 = LayerBuilder::new()
            .file("up/evil.txt", b"x", 0o644)
            .finish();
        let svc = make_bundle(&bundle, "app", vec![l0, l1]);

        assemble_rootfs_at(&bundle, &svc, &out).unwrap();
        // `..` 在 rootfs 根被夾住 → 檔案落在 rootfs 根
        assert!(out.join("evil.txt").exists());
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!tmp.path().parent().unwrap().join("evil.txt").exists());
    }

    // ── overlay 正確性：合成視圖必須等於合併樹 ──────────────────────────────
    // overlay 只改變 rootfs「怎麼組」（lazy、共享層、掛載瞬成），組出來的 rootfs
    // 語意應與既有合併路徑「逐檔一致」——掛載後的 bind/persist 兩條路完全相同，故
    // 「掛載出的 merged 視圖 == assemble_rootfs_at 合併樹」就是最強的正確性證明。
    //
    // 需要 real root（mknod whiteout char dev + mount overlay）：非 root 直接跳過，
    // 由 root CI job（e2e-linux.yml 的 overlay-rootfs-root-test）以 sudo 實跑。

    /// 對單一目錄樹做快照：相對路徑 → 型別/權限/內容雜湊（symlink 比目標、檔案比
    /// mode+sha256、目錄比 mode）。overlay 與合併兩樹比這份快照即可判等。
    fn snapshot(root: &Path) -> BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            entries.sort();
            for p in entries {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let md = std::fs::symlink_metadata(&p).unwrap();
                let ft = md.file_type();
                if ft.is_symlink() {
                    let tgt = std::fs::read_link(&p).unwrap();
                    out.insert(rel, format!("symlink -> {}", tgt.display()));
                } else if ft.is_dir() {
                    out.insert(rel, format!("dir {:04o}", md.permissions().mode() & 0o7777));
                    walk(root, &p, out);
                } else if ft.is_file() {
                    let content = std::fs::read(&p).unwrap();
                    let h = hex::encode(Sha256::digest(&content));
                    out.insert(
                        rel,
                        format!("file {:04o} {h}", md.permissions().mode() & 0o7777),
                    );
                } else {
                    // 乾淨的 rootfs 視圖不該出現 char/block/fifo/socket（whiteout 應被
                    // overlay 消化、不外露）；若出現即為 bug，記下供比對報錯。
                    out.insert(rel, format!("special {ft:?}"));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// 組一個組合：合併路徑 vs overlay 掛載視圖，斷言兩樹逐檔一致。
    /// 須在已 unshare(NEWNS) 且傳播設私有的執行緒上呼叫（見 overlay_view_matches_merge）。
    fn assert_overlay_matches_merge(name: &str, layers: Vec<(Vec<u8>, String)>) {
        use nix::mount::{MsFlags, mount, umount};

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let cache = tmp.path().join("cache");
        let svc = make_bundle(&bundle, "app", layers);

        // 合併樹（既有正確路徑，當作 ground truth）
        let merge = tmp.path().join("merge");
        assemble_rootfs_at(&bundle, &svc, &merge)
            .unwrap_or_else(|e| panic!("[{name}] merge 組裝失敗：{e:#}"));

        // overlay：各層 lowerdir（內容定址、共享）+ 本次可寫 upper/work + 掛載點 merged
        let lowers = assemble_overlay_layers(&bundle, &svc, &cache)
            .unwrap_or_else(|e| panic!("[{name}] overlay 層組裝失敗：{e:#}"));
        let upper = tmp.path().join("upper");
        let work = tmp.path().join("work");
        let merged = tmp.path().join("merged");
        for d in [&upper, &work, &merged] {
            std::fs::create_dir_all(d).unwrap();
        }
        // lowerdir 需上層在前（lowers 為 base→top，故反轉）——與 exec.rs 一致。
        let lowerdir = lowers
            .iter()
            .rev()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let opts = format!(
            "lowerdir={lowerdir},upperdir={},workdir={}",
            upper.display(),
            work.display()
        );
        mount(
            Some("overlay"),
            &merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(opts.as_str()),
        )
        .unwrap_or_else(|e| panic!("[{name}] overlay 掛載失敗：{e}（opts={opts}）"));

        let truth = snapshot(&merge);
        let view = snapshot(&merged);
        let _ = umount(&merged);

        if truth != view {
            let mut diff = String::new();
            let keys: std::collections::BTreeSet<&String> =
                truth.keys().chain(view.keys()).collect();
            for k in keys {
                let a = truth.get(k);
                let b = view.get(k);
                if a != b {
                    diff.push_str(&format!("\n  {k}: merge={a:?} overlay={b:?}"));
                }
            }
            panic!("[{name}] overlay 合成視圖與合併樹不一致：{diff}");
        }
    }

    #[test]
    fn overlay_view_matches_merge() {
        if !overlay_supported() {
            eprintln!(
                "skip overlay_view_matches_merge: overlay unsupported \
                 (needs real root + overlay fs; run via the root CI job)"
            );
            return;
        }

        // 全程在獨立 mount ns 內操作，並把傳播設私有 → overlay 掛載不外洩到 host。
        use nix::mount::{MsFlags, mount};
        use nix::sched::{CloneFlags, unshare};
        unshare(CloneFlags::CLONE_NEWNS).expect("unshare(NEWNS) failed");
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )
        .expect("set / propagation private failed");

        // 1) 單層、無 whiteout（最基本：檔案/目錄/symlink/權限）
        let basic = vec![
            LayerBuilder::new()
                .dir("etc", 0o755)
                .file("etc/conf", b"hello", 0o644)
                .dir("bin", 0o755)
                .file("bin/run", b"#!/bin/sh\n", 0o755)
                .symlink("etc/link", "conf")
                .finish(),
        ];
        assert_overlay_matches_merge("basic-single-layer", basic);

        // 2) whiteout 檔 + opaque 目錄 + 覆寫 + symlink + hardlink（沿用既有覆蓋面）
        let l0 = LayerBuilder::new()
            .dir("etc", 0o755)
            .file("etc/a.txt", b"v1", 0o644)
            .dir("data", 0o755)
            .file("data/x", b"x", 0o644)
            .file("data/y", b"y", 0o644)
            .dir("bin", 0o755)
            .file("bin/prog", b"#!/bin/sh\n", 0o755)
            .symlink("etc/link", "a.txt")
            .hardlink("etc/hard", "etc/a.txt")
            .finish();
        let l1 = LayerBuilder::new()
            .file("etc/.wh.a.txt", b"", 0o644)
            .file("data/.wh..wh..opq", b"", 0o644)
            .file("data/z", b"z", 0o600)
            .file("bin/prog", b"#!/bin/sh\necho v2\n", 0o700)
            .finish();
        assert_overlay_matches_merge("whiteout-opaque-overwrite", vec![l0, l1]);

        // 3) 刪除後再重建（whiteout 之上又出現同名檔 → 上層實檔應勝過下層 whiteout）
        let d0 = LayerBuilder::new()
            .dir("foo", 0o755)
            .file("foo/bar.txt", b"v1", 0o644)
            .finish();
        let d1 = LayerBuilder::new()
            .file("foo/.wh.bar.txt", b"", 0o644)
            .finish();
        let d2 = LayerBuilder::new()
            .file("foo/bar.txt", b"v3", 0o600)
            .finish();
        assert_overlay_matches_merge("delete-then-recreate", vec![d0, d1, d2]);

        // 4) 巢狀目錄 + 深層 opaque（opaque 應只清該目錄下層內容，保留同層新增）
        let n0 = LayerBuilder::new()
            .dir("a", 0o755)
            .dir("a/b", 0o755)
            .file("a/b/old1", b"1", 0o644)
            .file("a/b/old2", b"2", 0o644)
            .dir("a/b/c", 0o755)
            .file("a/b/c/deep", b"d", 0o644)
            .finish();
        let n1 = LayerBuilder::new()
            .file("a/b/.wh..wh..opq", b"", 0o644)
            .file("a/b/fresh", b"f", 0o644)
            .finish();
        assert_overlay_matches_merge("nested-deep-opaque", vec![n0, n1]);

        // 5) 多層連續覆寫同檔（內容與權限都應取最上層）
        let m0 = LayerBuilder::new().file("v", b"one", 0o644).finish();
        let m1 = LayerBuilder::new().file("v", b"two", 0o600).finish();
        let m2 = LayerBuilder::new().file("v", b"three", 0o755).finish();
        assert_overlay_matches_merge("multi-layer-overwrite", vec![m0, m1, m2]);

        // 6) 權限多樣性（各種 mode 的檔與目錄都應被 overlay 如實呈現）
        let p0 = LayerBuilder::new()
            .dir("d700", 0o700)
            .file("d700/secret", b"s", 0o600)
            .file("readonly", b"r", 0o444)
            .file("exec", b"e", 0o755)
            .dir("d555", 0o555)
            .finish();
        assert_overlay_matches_merge("permission-variety", vec![p0]);

        // 成功標記：root CI job 會斷言這行出現（且未出現 "skip ..."），避免
        // overlay 不支援時測試靜默跳過卻讓 job 變綠（假陽性）。
        eprintln!("overlay_view_matches_merge: OK (6 combos verified overlay == merge)");
    }
}
