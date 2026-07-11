//! VM guest 的 GUI 顯示環境（DESIGN §6「GUI overlay 打包契約」「WHP GUI」）。
//!
//! 原生 Linux / WSL2 走「bind host compositor socket」路線（exec.rs 既有邏輯）；
//! micro-VM（WHP / vz）內沒有 host compositor——由本模組在 guest 內自建：
//!
//! 1. 唯讀掛載 bundle `vm/chefer-gui-overlay-<arch>.sqfs`（squashfs：cage + Xwayland +
//!    Mesa llvmpipe + eudev 及依賴閉包，Alpine 基底），再以 overlayfs 把其中各 top-level
//!    目錄疊上 VM 的 tmpfs 根（appliance init 已 switch_root 到 tmpfs 根）——**免每次開機
//!    解壓 200MB+**，squashfs 頁面按需載入。
//! 2. 啟動 `udevd` 並 trigger（wlroots 的 DRM/libinput 裝置探索需要 udev）、
//!    `seatd`（Alpine 的 libseat 沒編 builtin backend）。
//! 3. 以 `LIBSEAT_BACKEND=seatd` 啟動 `cage`（kiosk Wayland compositor；子行程給
//!    長眠 sleep 佔位，介面服務其後以一般 Wayland/X11 client 連上），等待
//!    `$XDG_RUNTIME_DIR/wayland-0`（與 Xwayland 的 `/tmp/.X11-unix/X0`）出現。
//! 4. 設定 `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`/`DISPLAY` 供 exec.rs 既有的
//!    GUI socket bind 邏輯把 socket 掛進服務容器。
//! 5. 啟動 host↔guest 剪貼簿同步（clipboard.rs）與動態解析度跟隨（resize.rs——
//!    host 調整視窗 → guest re-modeset，cage 本身不跟隨線上模式變更）。
//!
//! 觸發條件（`maybe_start`）：appliance init 設 `CHEFER_VM_GUI=1` **且** manifest
//! 有 gui/both 服務。overlay 缺失 → 硬錯誤（依契約不得無聲黑屏）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chefer_bundle::Manifest;
use nix::mount::{MsFlags, mount};

/// 記一行 GUI 診斷。同時走 stderr（原生 Linux / 有接 console 的後端可見）與 **`/dev/kmsg`**
/// ——WHP 的序列 console 在 `quiet` 下不顯示 guest-agent 自己的 stderr，但高優先序 kmsg 會
/// 出現在 console（同 appliance init 的 `report()`），故 WHP GUI 的啟動診斷才看得到。best-effort。
pub(crate) fn note(msg: &str) {
    eprintln!("[guest-agent] gui: {msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        use std::io::Write as _;
        // **一次 write() = 一筆 kmsg 記錄**：必須先組好整行再單次寫入。`writeln!`（write_fmt）
        // 會拆成多次 write，只有第一筆帶 `<2>` 前綴、其餘（含訊息本體）落回預設 level 而被
        // `quiet` 擋掉。`quiet` 把 console_loglevel 壓到 4（只印 level < 4），故用 <2>=KERN_CRIT
        //（appliance init 的 report() 用 <0> 亦同理）。
        let line = format!("<2>[guest-agent] gui: {msg}\n");
        let _ = f.write_all(line.as_bytes());
    }
}

/// cage 建立 Wayland socket 的 runtime 目錄（guest 內，appliance tmpfs 上）。
const GUI_RUNTIME_DIR: &str = "/run/chefer-gui";
/// 等 compositor socket 出現的上限。llvmpipe 首次啟動慢，放寬一點。
const SOCKET_WAIT: Duration = Duration::from_secs(20);

/// 存活中的 GUI 環境；Drop 時收掉 resize/clipboard / cage / seatd / udevd。
pub struct GuiSession {
    cage: Child,
    seatd: Option<Child>,
    udevd: Option<Child>,
    /// host↔guest 剪貼簿同步（有 cmdline token 且 wl-clipboard 可用時；見 clipboard.rs）。
    clipboard: Option<crate::clipboard::ClipboardSync>,
    /// 動態解析度跟隨（wlr-randr 可用時；見 resize.rs）。
    resize: Option<crate::resize::ResizeWatcher>,
}

impl Drop for GuiSession {
    fn drop(&mut self) {
        // 先收背景執行緒（解析度跟隨、剪貼簿同步），再收 compositor。
        self.resize.take();
        self.clipboard.take();
        let _ = self.cage.kill();
        let _ = self.cage.wait();
        for c in [self.seatd.take(), self.udevd.take()].into_iter().flatten() {
            let mut c = c;
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 需要時建立 VM 內 GUI 環境。回傳 `Ok(None)` 表示不適用（非 VM GUI 情境）。
pub fn maybe_start(bundle_dir: &Path, manifest: &Manifest) -> Result<Option<GuiSession>> {
    if std::env::var_os("CHEFER_VM_GUI").is_none_or(|v| v != "1") {
        return Ok(None);
    }
    if !manifest
        .services
        .iter()
        .any(|s| s.interface_mode.wants_gui())
    {
        return Ok(None);
    }

    let overlay = chefer_bundle::layout::vm_dir(bundle_dir).join(
        chefer_bundle::layout::gui_overlay_name(std::env::consts::ARCH),
    );
    if !overlay.is_file() {
        // 契約：缺 overlay 的 GUI app 在 VM 後端必須得到可行動錯誤，不得無聲黑屏。
        bail!(
            "this app has a GUI service, but the bundle has no GUI overlay ({}). \
             It was probably packed with a kit that lacks chefer-gui-overlay-{} \
             (see the warning printed by `chefer build`). Repack with a complete kit \
             (built by scripts/build-gui-overlay.sh, shipped in releases) to show a GUI here.",
            overlay.display(),
            std::env::consts::ARCH,
        );
    }

    note("mounting the GUI overlay (cage/Xwayland/Mesa squashfs) …");
    mount_overlay(&overlay).context("failed to mount the GUI overlay")?;

    note("starting udev + seatd");
    let udevd = start_udev();
    let seatd = start_seatd();
    note("starting cage");
    let cage = start_cage().context("failed to start the in-VM compositor (cage)")?;
    let mut session = GuiSession {
        cage,
        seatd,
        udevd,
        clipboard: None,
        resize: None,
    };

    let wayland = PathBuf::from(GUI_RUNTIME_DIR).join("wayland-0");
    wait_for_socket(&wayland, &mut session)?;
    // SAFETY: 服務尚未 spawn、supervisor 執行緒尚未啟動的單執行緒早期階段。
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", GUI_RUNTIME_DIR);
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
    }
    // Xwayland（X11 app 支援）：cage/wlroots 啟用時會先建立 X0 socket（lazy 啟動仍先佔位）。
    // 沒編 Xwayland 的 cage 也不致命——純 Wayland app 仍可用，X11 app 會連不到 DISPLAY。
    let x0 = Path::new("/tmp/.X11-unix/X0");
    if x0.exists() {
        // SAFETY: 同上，仍在單執行緒階段。
        // access control 已由 overlay 的 Xwayland shim 以 `-ac` 關閉（安全邊界是
        // micro-VM 本身；實測不帶 cookie 的同 uid 本機連線也會被拒，見 build-gui-overlay.sh）。
        unsafe { std::env::set_var("DISPLAY", ":0") };
    } else {
        eprintln!(
            "[guest-agent] gui: Xwayland socket not present; X11-only apps will not find a DISPLAY (Wayland apps are unaffected)"
        );
    }
    note("compositor ready (WAYLAND_DISPLAY=wayland-0)");
    // compositor 就緒後起剪貼簿同步（env 已設好，wl-clipboard 可接 cage）。無 cmdline
    // token（host 未啟用剪貼簿）或 wl-clipboard 缺失時回 None，不致命。
    session.clipboard = crate::clipboard::maybe_start();
    // 動態解析度跟隨（host 調整視窗 → guest re-modeset；見 resize.rs）。wlr-randr 缺失
    //（舊 overlay）時回 None，不致命——退回「host 端拉伸顯示」的舊行為。
    session.resize = crate::resize::maybe_start();
    Ok(Some(session))
}

/// overlay 掛載時**不**疊上去的 top-level 目錄：虛擬/掛載點與 appliance 執行期用到的目錄
/// （尤其 `/run` 內有 guest-agent 執行檔、`/tmp` 內有 rootfs 快取與本 overlay 的 work dir）。
const OVERLAY_SKIP: &[&str] = &["dev", "proc", "sys", "run", "tmp", "mnt", "newroot", "boot"];

/// 掛載 GUI overlay（squashfs），並以 overlayfs 把其中各 top-level 目錄疊上 VM 的 tmpfs 根。
///
/// 取代舊的「每次開機把 tar.zst 解壓 200MB+ 到 `/`」：squashfs 唯讀掛一次、頁面按需載入，
/// overlay 讓 overlay 內的檔案出現在既有 `/usr`、`/lib`、`/etc` 等路徑（overlay 內容為高
/// 優先 lower、原內容為低優先 lower、可寫層 upper 放 tmpfs）。免複製、掛載瞬間完成。
fn mount_overlay(sqfs: &Path) -> Result<()> {
    let ro = Path::new("/run/chefer/gui-ro");
    std::fs::create_dir_all(ro).with_context(|| format!("failed to create {}", ro.display()))?;
    // squashfs 是 block filesystem——掛載檔案須先綁一個 loop 裝置（不能直接 mount 檔案）。
    let loop_dev = setup_loop(sqfs)?;
    mnt(
        Some(&loop_dev),
        ro,
        Some("squashfs"),
        MsFlags::MS_RDONLY,
        None,
    )
    .with_context(|| {
        format!(
            "failed to mount the GUI overlay squashfs {} via {} \
             (the appliance kernel needs CONFIG_SQUASHFS + CONFIG_SQUASHFS_ZSTD)",
            sqfs.display(),
            loop_dev.display()
        )
    })?;

    // overlay 的 orig 參考點與可寫 upper/work 放 tmpfs（/tmp 已由 rootfs overlay 驗證可當 upper）。
    let base = Path::new("/tmp/chefer-gui-overlay");
    let mut overlaid = 0usize;
    for entry in
        std::fs::read_dir(ro).with_context(|| format!("failed to read {}", ro.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if OVERLAY_SKIP.contains(&name.as_str()) {
            continue;
        }
        let lower_gui = ro.join(&name);
        let target = Path::new("/").join(&name);
        let _ = std::fs::create_dir_all(&target);
        let orig = base.join(&name).join("orig");
        let upper = base.join(&name).join("upper");
        let work = base.join(&name).join("work");
        for d in [&orig, &upper, &work] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("failed to create overlay dir {}", d.display()))?;
        }
        // 先把目標原內容 bind 到 orig（overlay 不能拿 mountpoint 自己當 lowerdir）。
        mnt(
            Some(&target),
            &orig,
            None,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None,
        )
        .with_context(|| format!("failed to bind {} aside for overlay", target.display()))?;
        // gui（squashfs）為高優先 lower、原內容為低優先 lower。
        let opts = format!(
            "lowerdir={}:{},upperdir={},workdir={}",
            lower_gui.display(),
            orig.display(),
            upper.display(),
            work.display()
        );
        mnt(
            Some(Path::new("overlay")),
            &target,
            Some("overlay"),
            MsFlags::empty(),
            Some(opts.as_str()),
        )
        .with_context(|| format!("failed to overlay GUI files onto {}", target.display()))?;
        overlaid += 1;
    }
    if overlaid == 0 {
        bail!(
            "the GUI overlay squashfs {} contained no usable directories to mount",
            sqfs.display()
        );
    }
    Ok(())
}

/// `nix::mount::mount` 的薄封裝（比照 exec.rs），固定各引數型別以免 turbofish。
fn mnt(
    src: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> nix::Result<()> {
    mount(src, target, fstype, flags, data)
}

// linux/loop.h ioctl 常數。
const LOOP_SET_FD: u64 = 0x4C00;
const LOOP_CTL_GET_FREE: u64 = 0x4C82;

/// 把 squashfs 檔綁到一個 free loop 裝置，回傳其 `/dev/loopN` 路徑（唯讀：backing 以
/// `O_RDONLY` 開，loop 因而為唯讀）。kernel 需 `CONFIG_BLK_DEV_LOOP`。VM ephemeral，
/// 不需手動 `LOOP_CLR_FD`（關機即釋放）。
fn setup_loop(sqfs: &Path) -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let backing = std::fs::File::open(sqfs)
        .with_context(|| format!("failed to open GUI overlay {}", sqfs.display()))?;
    let ctl = std::fs::File::open("/dev/loop-control").context(
        "failed to open /dev/loop-control (the appliance kernel needs CONFIG_BLK_DEV_LOOP)",
    )?;
    let num = unsafe { libc::ioctl(ctl.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
    if num < 0 {
        return Err(std::io::Error::last_os_error()).context("LOOP_CTL_GET_FREE ioctl failed");
    }
    let loop_path = PathBuf::from(format!("/dev/loop{num}"));
    // loop 節點以 O_RDWR 開（losetup 慣例，LOOP_SET_FD 相容性較廣）；唯讀性由 backing 的
    // O_RDONLY 決定（kernel 讓 loop 繼承 backing 的唯讀屬性），故實際仍是唯讀 loop。
    let loopdev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&loop_path)
        .with_context(|| format!("failed to open {}", loop_path.display()))?;
    let r = unsafe { libc::ioctl(loopdev.as_raw_fd(), LOOP_SET_FD as _, backing.as_raw_fd()) };
    if r < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("LOOP_SET_FD on {} failed", loop_path.display()));
    }
    // LOOP_SET_FD 後 kernel 已持有 backing 檔參考；backing/loopdev fd 於此可關（loop 綁定持續）。
    Ok(loop_path)
}

/// 啟動 udevd 並 trigger 一輪裝置事件（best-effort：udev 缺失時 wlroots 多半仍可
/// 直接開 /dev/dri，僅 libinput 熱插拔受影響——印訊息不致命）。
fn start_udev() -> Option<Child> {
    let udevd = ["/sbin/udevd", "/usr/sbin/udevd", "/sbin/eudevd"]
        .iter()
        .find(|p| Path::new(p).is_file())?;
    let child = Command::new(udevd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("[guest-agent] gui: udevd failed to start ({e}); continuing"))
        .ok()?;
    for args in [
        &["trigger", "--action=add"][..],
        &["settle", "--timeout=5"][..],
    ] {
        let _ = Command::new("/bin/udevadm")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    Some(child)
}

/// 啟動 seatd（libseat 的 seat 管理 daemon）。Alpine 的 libseat 沒編 builtin backend
/// （實測 cage 直接退出：`No backend matched name 'builtin'`），故起 seatd、等它的
/// socket 就緒。失敗回 None（cage 隨後會失敗並給出可行動錯誤）。
fn start_seatd() -> Option<Child> {
    let seatd = ["/usr/bin/seatd", "/bin/seatd"]
        .iter()
        .find(|p| Path::new(p).is_file())?;
    let child = Command::new(seatd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| eprintln!("[guest-agent] gui: seatd failed to start ({e})"))
        .ok()?;
    // 等 /run/seatd.sock 出現（最多 3 秒；沒等到也讓 cage 自己再試）。
    let sock = Path::new("/run/seatd.sock");
    let start = Instant::now();
    while !sock.exists() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(child)
}

/// 啟動 cage。子行程用長眠 sleep 佔位（cage 需要一個 client 引數；真正的介面服務
/// 稍後以一般 client 連上 Wayland/X11 socket）。
fn start_cage() -> Result<Child> {
    let cage = ["/usr/bin/cage", "/bin/cage"]
        .iter()
        .find(|p| Path::new(p).is_file())
        .context("cage not found after mounting the GUI overlay (overlay is incomplete?)")?;
    std::fs::create_dir_all(GUI_RUNTIME_DIR)?;
    let mut perm = std::fs::metadata(GUI_RUNTIME_DIR)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    perm.set_mode(0o700);
    std::fs::set_permissions(GUI_RUNTIME_DIR, perm)?;

    let child = Command::new(cage)
        .arg("--")
        .arg("/bin/sleep")
        .arg("2147483647")
        .env("XDG_RUNTIME_DIR", GUI_RUNTIME_DIR)
        .env("WLR_RENDERER", "pixman") // VM 無 GPU：強制軟算繪，避免 GLES 探測失敗
        .env("LIBSEAT_BACKEND", "seatd") // Alpine libseat 無 builtin backend → 走 seatd
        // 實機根因（WHP GUI 約 1/3 間歇失敗）：virtio-input 節點（/dev/input/event*）由 udev
        // 非同步建立，cage 若早於 udev 啟動，wlroots 的 libinput backend 見「no input devices」
        // 直接 abort → cage 秒退。wlroots 的錯誤訊息本身建議設此旗標容忍 0 輸入裝置啟動
        //（裝置隨後由 udev 熱插拔補上），消除此競態。
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .stdin(Stdio::null())
        // cage/wlroots 的啟動失敗原因（缺 DRM、keymap、seatd…）只會出現在 stderr——**捕獲**
        // 起來，cage 若啟動失敗（wait_for_socket 偵測）就轉發到 /dev/kmsg（WHP console 可見；
        // 否則在 `quiet` 下這段錯誤完全看不到，見 note()）。
        .stderr(Stdio::piped())
        .spawn()?;
    Ok(child)
}

/// 等 compositor socket 出現；cage 提前退出時把 exit status 化為錯誤。
fn wait_for_socket(sock: &Path, session: &mut GuiSession) -> Result<()> {
    let start = Instant::now();
    loop {
        if sock.exists() {
            return Ok(());
        }
        if let Some(status) = session.cage.try_wait()? {
            // cage 啟動失敗——把它捕獲的 stderr 轉到 kmsg（WHP console 可見），這正是
            // 「Found 0 GPUs」/ seatd / keymap 之類真正原因的所在，否則在 quiet 下看不到。
            let mut cage_err = String::new();
            if let Some(mut err) = session.cage.stderr.take() {
                use std::io::Read as _;
                let _ = err.read_to_string(&mut cage_err);
            }
            let cage_err = cage_err.trim();
            if !cage_err.is_empty() {
                for line in cage_err.lines() {
                    note(&format!("cage: {line}"));
                }
            }
            bail!(
                "the in-VM compositor (cage) exited during startup ({status}); GUI cannot be shown. \
                 cage stderr: {}. \
                 (missing /dev/dri usually means the appliance kernel lacks virtio-gpu, or the VM \
                 host did not attach a virtio-gpu device.)",
                if cage_err.is_empty() {
                    "(none captured)"
                } else {
                    cage_err
                }
            );
        }
        if start.elapsed() > SOCKET_WAIT {
            bail!(
                "timed out waiting for the compositor socket {} ({}s); GUI cannot be shown",
                sock.display(),
                SOCKET_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
