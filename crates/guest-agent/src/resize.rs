//! VM guest 的「執行中」動態解析度跟隨（DESIGN §6「WHP GUI」動態解析度 / macOS 分期④）。
//!
//! host 端已把新尺寸推進 guest：WHP 於使用者調整視窗時經 virtio-gpu config 中斷更新
//! display info/EDID（roadmap M8-d 實機驗證）；vz 的 `automaticallyReconfiguresDisplay`
//! 亦同（macOS 14+）。guest 的 virtio-gpu driver 收到後重抓 display info 並發 DRM
//! hotplug uevent——但 cage（kiosk compositor）不會因「已連線 connector 的模式清單變更」
//! re-modeset：wlroots 對此的通知（preferred mode 變更 → `request_state`）在上游仍是
//! 未合併的 draft（wlroots MR 5182，目標 0.20），Alpine 出貨版短期拿不到。
//!
//! 本模組補上這一段：
//! 1. 監聽 kernel uevent（netlink `NETLINK_KOBJECT_UEVENT`，group 1 直聽 kernel、
//!    不依賴 udevd 轉播）等 drm 子系統的 change 事件（拖拉中會連發 → debounce 收斂）。
//! 2. 以 `DRM_IOCTL_MODE_GETCONNECTOR` 的 **count_modes=0 首呼叫**觸發 kernel 對
//!    connector 重新 probe，取得新的 preferred mode 尺寸——不能讀 sysfs `modes`，
//!    它只反映「上次 probe」的結果，沒人 probe 就永遠是舊值。
//! 3. 呼叫 overlay 內的 `wlr-randr --custom-mode` 經 **wlr-output-management** 協定要求
//!    cage 換模式。cage 0.2.0+（Alpine 3.21+）已實作該協定，apply 走 wlroots 的
//!    `wlr_output_head_v1_state_apply`——支援 custom mode，因此**繞過 cage 端已過期的
//!    模式清單**（wlroots 不會為已連線 connector 更新 wlr_output 的 mode list）。
//!    cage commit 新模式 → wlr_output_layout 變更 → view 重排滿版，Xwayland root 同步。
//!
//! **HiDPI（`chefer.gui_scale=`）**：vz 動態解析度模式推進 guest 的是 Retina **實體像素**
//! 尺寸，Linux guest 無 HiDPI 概念 → UI/游標縮半。host（vz-helper）把開機當下螢幕的
//! backingScaleFactor 附進 kernel cmdline，本 watcher 據此對輸出設 wlr-output-management
//! 的 **output scale**：啟動時先補設一次（開機初始模式就已是像素尺寸、之後未必再有 drm
//! 事件），此後每次換模式一併重設。邏輯尺寸回到「點」數；X11 surface 由 compositor 放大
//! （預期視覺等同預設 view-scaling，但比例自由、無 letterbox、游標尺寸正常——待實機驗證）。
//! 未附此參數（WHP、vz 預設模式）→ 完全不帶 `--scale`，行為與既往相同。
//!
//! **為何不是 replug**：曾試「connector 回報 disabled→enabled」模擬拔插，cage 會拆輸出
//! → 介面 app 退出 → fail_fast 收掉整個 app（見 roadmap M8-d），故必須走「connector
//! 保持連線、只換模式」的本路線。等 wlroots 0.20 + 對應 cage 進 Alpine 後，cage 會自己
//! 跟隨 `request_state`，本 watcher 變成冗餘但無害（同尺寸的再設一次會被抑制）。

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// uevent 接收逾時（= stop 旗標的輪詢間隔，也是 debounce 迴圈的步長）。
const RECV_TIMEOUT: Duration = Duration::from_millis(200);
/// drm change 事件後的靜默窗：拖拉過程 host 連發 config 事件，等這麼久沒有新事件才動作。
const DEBOUNCE: Duration = Duration::from_millis(400);
/// wlr-randr 連續失敗這麼多次就放棄（例如舊 overlay 的 cage 0.1.x 沒有
/// wlr-output-management 協定——重試不會好，別每次拖拉都刷錯誤）。
const MAX_FAILURES: u32 = 3;

/// 存活中的解析度跟隨；Drop 時通知背景執行緒收尾（同 clipboard.rs 模式）。
pub struct ResizeWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ResizeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 需要時啟動解析度跟隨。回 None 表示不適用：overlay 沒有 wlr-randr（舊 kit）或
/// netlink socket 開不起來。呼叫端須先設好 `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`
/// （wlr-randr 子行程繼承本行程環境）。
pub fn maybe_start() -> Option<ResizeWatcher> {
    if crate::clipboard::which("wlr-randr").is_none() {
        crate::gui::note(
            "resize: wlr-randr not found in the GUI overlay; dynamic resolution disabled \
             (repack with a kit built from a newer build-gui-overlay.sh)",
        );
        return None;
    }
    let sock = match open_uevent_socket() {
        Ok(s) => s,
        Err(e) => {
            crate::gui::note(&format!(
                "resize: cannot listen for kernel uevents ({e}); dynamic resolution disabled"
            ));
            return None;
        }
    };
    let scale = read_cmdline_gui_scale();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let handle = std::thread::Builder::new()
        .name("chefer-resize".into())
        .spawn(move || run(sock, stop_c, scale))
        .ok()?;
    match scale {
        Some(s) => crate::gui::note(&format!(
            "resize: following host window size (drm uevent -> wlr-output-management); HiDPI output scale {s}"
        )),
        None => crate::gui::note(
            "resize: following host window size (drm uevent -> wlr-output-management)",
        ),
    }
    Some(ResizeWatcher {
        stop,
        handle: Some(handle),
    })
}

/// 主迴圈：等 drm change → debounce → probe 新 preferred mode → wlr-randr 套用。
fn run(sock: OwnedFd, stop: Arc<AtomicBool>, scale: Option<f64>) {
    // HiDPI：開機初始模式就已是 Retina 像素尺寸（vz 於開機即推入、cage 啟動即選中），
    // 之後未必再有 drm 事件——啟動時就對現行輸出補設 output scale 一次。
    if let Some(s) = scale {
        match apply_scale(s) {
            Ok(output) => crate::gui::note(&format!("resize: {output} output scale -> {s}")),
            Err(e) => crate::gui::note(&format!("resize: failed to set output scale {s}: {e}")),
        }
    }
    // 最近一次成功套用的尺寸：同尺寸不重設（避免無謂 re-modeset 閃爍）。
    let mut last: Option<(u16, u16)> = None;
    let mut failures = 0u32;
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        match recv_uevent(&sock, &mut buf) {
            RecvOutcome::Quiet => continue,
            RecvOutcome::DrmChange => {}
        }
        // debounce：等到 DEBOUNCE 內沒有新的 drm 事件才動作（拖拉中 host 連發）。
        let mut last_event = Instant::now();
        while last_event.elapsed() < DEBOUNCE {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if matches!(recv_uevent(&sock, &mut buf), RecvOutcome::DrmChange) {
                last_event = Instant::now();
            }
        }

        let Some((w, h)) = probe_preferred_size() else {
            crate::gui::note("resize: drm change event, but no connected connector with modes");
            continue;
        };
        if last == Some((w, h)) {
            continue;
        }
        match apply_mode(w, h, scale) {
            Ok(output) => {
                last = Some((w, h));
                failures = 0;
                match scale {
                    Some(s) => {
                        crate::gui::note(&format!("resize: {output} -> {w}x{h} (scale {s})"));
                    }
                    None => crate::gui::note(&format!("resize: {output} -> {w}x{h}")),
                }
            }
            Err(e) => {
                failures += 1;
                crate::gui::note(&format!("resize: failed to apply {w}x{h}: {e}"));
                if failures >= MAX_FAILURES {
                    crate::gui::note(
                        "resize: giving up (wlr-randr keeps failing; is the overlay's cage \
                         older than 0.2.0, i.e. without wlr-output-management?)",
                    );
                    return;
                }
            }
        }
    }
}

// ---- HiDPI output scale（kernel cmdline）----

/// 從 `/proc/cmdline` 取 host 附上的 `chefer.gui_scale=`（vz 動態解析度模式限定，
/// 見 vz-helper/main.swift；同 clipboard.rs 讀 clip_token 的模式）。無或無效 → None。
fn read_cmdline_gui_scale() -> Option<f64> {
    parse_gui_scale(&std::fs::read_to_string("/proc/cmdline").ok()?)
}

/// 解析 `chefer.gui_scale=` 值。僅接受 (1.0, 4.0] 的有限值：1 = 不需縮放（省略
/// `--scale`，與未附參數完全同路徑）；範圍外視為 host 端錯誤——寧可不縮放，也不把
/// 0/負值/NaN 這類會直接毀掉版面的值餵給 compositor。
fn parse_gui_scale(cmdline: &str) -> Option<f64> {
    let v = cmdline
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("chefer.gui_scale="))?;
    let s: f64 = v.parse().ok()?;
    (s.is_finite() && s > 1.0 && s <= 4.0).then_some(s)
}

// ---- kernel uevent（netlink）----

/// 開 `NETLINK_KOBJECT_UEVENT` socket 聽 kernel 原生 uevent 廣播（group 1）。
/// udevd 的轉播在 group 2 且是 libudev 私有格式——直聽 kernel 不依賴 udevd 存活。
fn open_uevent_socket() -> std::io::Result<OwnedFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = 0; // 0 = kernel 自派唯一 id（勿填 getpid：一個行程可能開多個 netlink socket）
    addr.nl_groups = 1; // kernel uevent 廣播群組
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            std::ptr::from_ref(&addr).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // recv timeout 讓 stop 旗標能中止，同時充當 debounce 迴圈步長。
    // tv_usec 的欄位型別在 musl 1.2 改為 64-bit（libc 的 suseconds_t 別名已標 deprecated）
    // ——以 `as _` 交給推導，兩邊都對。
    let tv = libc::timeval {
        tv_sec: 0,
        tv_usec: RECV_TIMEOUT.subsec_micros() as _,
    };
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::from_ref(&tv).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

enum RecvOutcome {
    /// 逾時、非 drm 事件、或可忽略的錯誤。
    Quiet,
    /// 收到 drm 子系統的 change 事件（或 ENOBUFS 溢位——可能漏了事件，保守當作有）。
    DrmChange,
}

/// 收一則 uevent 並分類。
fn recv_uevent(sock: &OwnedFd, buf: &mut [u8]) -> RecvOutcome {
    let n = unsafe {
        libc::recv(
            sock.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            0,
        )
    };
    if n < 0 {
        // ENOBUFS：kernel 端佇列溢位（我們聽的是全系統 uevent）——期間可能漏掉 drm
        // 事件，保守觸發一次 probe（同尺寸抑制讓誤觸發無害）。
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
            return RecvOutcome::DrmChange;
        }
        return RecvOutcome::Quiet; // EAGAIN（逾時）/EINTR
    }
    if is_drm_change(&buf[..n as usize]) {
        RecvOutcome::DrmChange
    } else {
        RecvOutcome::Quiet
    }
}

/// kernel uevent 格式：`ACTION@DEVPATH\0KEY=VAL\0…`。判斷是否 drm 子系統的 change 事件
/// （virtio-gpu 的 display info 變更走 `drm_kms_helper_hotplug_event` → change uevent）。
fn is_drm_change(msg: &[u8]) -> bool {
    let mut parts = msg.split(|b| *b == 0);
    // 首段是 header；udevd group 2 轉播的 "libudev" magic 開頭在此自然不匹配。
    let Some(header) = parts.next() else {
        return false;
    };
    if !header.starts_with(b"change@") {
        return false;
    }
    parts.any(|p| p == b"SUBSYSTEM=drm")
}

// ---- DRM probe（raw ioctl，同 gui.rs loop 裝置的作法，免引依賴）----

/// linux/drm_mode.h `struct drm_mode_card_res`。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// linux/drm_mode.h `struct drm_mode_modeinfo`（68 bytes）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeModeinfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    typ: u32,
    name: [u8; 32],
}

/// linux/drm_mode.h `struct drm_mode_get_connector`（80 bytes）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `_IOWR('d', nr, T)`：dir(READ|WRITE)=3 << 30 | sizeof(T) << 16 | 'd'(0x64) << 8 | nr。
const fn drm_iowr(nr: u64, size: usize) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | (0x64 << 8) | nr
}
const DRM_IOCTL_MODE_GETRESOURCES: u64 = drm_iowr(0xA0, std::mem::size_of::<DrmModeCardRes>());
const DRM_IOCTL_MODE_GETCONNECTOR: u64 = drm_iowr(0xA7, std::mem::size_of::<DrmModeGetConnector>());
/// drm_mode.h `DRM_MODE_CONNECTED`。
const DRM_MODE_CONNECTED: u32 = 1;
/// drm_mode.h `DRM_MODE_TYPE_PREFERRED`。
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;

/// EINTR/EAGAIN 重試的 ioctl 薄封裝（libdrm 慣例：probe 期間被訊號打斷很常見）。
fn drm_ioctl<T>(fd: RawFd, req: u64, val: &mut T) -> std::io::Result<()> {
    loop {
        let r = unsafe { libc::ioctl(fd, req as _, std::ptr::from_mut(val)) };
        if r == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(err),
        }
    }
}

/// 對 `/dev/dri/card*` 逐一 probe，回傳第一個「已連線 connector」的 preferred mode 尺寸
/// （無 preferred 旗標就取第一個 mode）。VM 內單 GPU 單 scanout，第一個命中即答案。
fn probe_preferred_size() -> Option<(u16, u16)> {
    for n in 0..8 {
        let Ok(card) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/dri/card{n}"))
        else {
            continue;
        };
        if let Some(size) = probe_card(card.as_raw_fd()) {
            return Some(size);
        }
    }
    None
}

/// 一張卡：GETRESOURCES 列 connector → 逐一 GETCONNECTOR。
/// GETCONNECTOR 的 **count_modes=0 首呼叫**會讓 kernel 對 connector 重新 probe——
/// virtio-gpu 藉此把 host 剛推來的新尺寸建成新的 preferred mode；第二呼叫帶緩衝區
/// 取回清單（不再 probe）。
///
/// 兩段式呼叫之間清單可能又變（再一次 hotplug）：此時 kernel **只回報新數量、不複製**
/// ——緩衝區會是全零。照 libdrm 慣例「數量對不上就整段重來」（上限 3 次）。
fn probe_card(fd: RawFd) -> Option<(u16, u16)> {
    'retry: for _ in 0..3 {
        let mut res = DrmModeCardRes::default();
        drm_ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res).ok()?;
        if res.count_connectors == 0 {
            return None;
        }
        let mut ids = vec![0u32; res.count_connectors as usize];
        let mut res2 = DrmModeCardRes {
            connector_id_ptr: ids.as_mut_ptr() as u64,
            count_connectors: ids.len() as u32,
            ..Default::default()
        };
        drm_ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res2).ok()?;
        if (res2.count_connectors as usize) > ids.len() {
            continue 'retry; // 清單變長，kernel 沒複製
        }
        ids.truncate(res2.count_connectors as usize);

        for &conn_id in &ids {
            // 首呼叫：count_modes=0 → kernel probe 並回報數量與連線狀態。
            let mut conn = DrmModeGetConnector {
                connector_id: conn_id,
                ..Default::default()
            };
            if drm_ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).is_err() {
                continue;
            }
            if conn.connection != DRM_MODE_CONNECTED || conn.count_modes == 0 {
                continue;
            }
            // 第二呼叫：只帶 modes 緩衝區（props/encoders 給 0 = 不取，也不再 probe）。
            let mut modes = vec![DrmModeModeinfo::default(); conn.count_modes as usize];
            let mut conn2 = DrmModeGetConnector {
                connector_id: conn_id,
                modes_ptr: modes.as_mut_ptr() as u64,
                count_modes: modes.len() as u32,
                ..Default::default()
            };
            if drm_ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn2).is_err() {
                continue;
            }
            if (conn2.count_modes as usize) > modes.len() {
                continue 'retry; // 兩次呼叫間又 hotplug，kernel 沒複製
            }
            modes.truncate(conn2.count_modes as usize);
            if let Some(size) = pick_mode(&modes) {
                return Some(size);
            }
        }
        return None;
    }
    None
}

/// 取 preferred mode（無則第一個可用者）的 (寬, 高)。零尺寸的項目不採
/// （守住「把 0x0 交給 wlr-randr」的下游災難）。
fn pick_mode(modes: &[DrmModeModeinfo]) -> Option<(u16, u16)> {
    let usable = |m: &&DrmModeModeinfo| m.hdisplay > 0 && m.vdisplay > 0;
    modes
        .iter()
        .filter(usable)
        .find(|m| m.typ & DRM_MODE_TYPE_PREFERRED != 0)
        .or_else(|| modes.iter().find(usable))
        .map(|m| (m.hdisplay, m.vdisplay))
}

// ---- 套用（wlr-randr → wlr-output-management → cage）----

/// 從 compositor 取輸出名。output 名以 compositor 回報為準（kiosk 單輸出；
/// virtio-gpu 下慣例是 Virtual-1，但不硬編——從 wlr-randr 列表拿第一個）。
fn current_output_name() -> Result<String, String> {
    let (stdout, stderr, ok) = run_wlr_randr(&[])?;
    if !ok {
        return Err(format!("wlr-randr list failed: {}", stderr.trim()));
    }
    first_output_name(&stdout).ok_or_else(|| "wlr-randr reported no outputs".to_string())
}

/// 以 custom mode 要求 cage 換到 `w`×`h`（refresh 交給 compositor 預設）；有 HiDPI
/// scale 時同一次 apply 一併重設。回傳套用到的 output 名。
fn apply_mode(w: u16, h: u16, scale: Option<f64>) -> Result<String, String> {
    let name = current_output_name()?;
    let args = mode_args(&name, w, h, scale);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (_, stderr, ok) = run_wlr_randr(&args)?;
    if !ok {
        return Err(format!("wlr-randr --custom-mode failed: {}", stderr.trim()));
    }
    Ok(name)
}

/// 只設 output scale、模式不動（watcher 啟動時對開機初始模式補設用）。回傳 output 名。
fn apply_scale(scale: f64) -> Result<String, String> {
    let name = current_output_name()?;
    let (_, stderr, ok) = run_wlr_randr(&["--output", &name, "--scale", &format!("{scale}")])?;
    if !ok {
        return Err(format!("wlr-randr --scale failed: {}", stderr.trim()));
    }
    Ok(name)
}

/// 組「換模式（有 scale 一併設）」的 wlr-randr 引數。`scale=None` → 不帶 `--scale`，
/// 與既往完全相同（未附 `chefer.gui_scale` 的路徑：WHP、vz 預設模式）。
fn mode_args(output: &str, w: u16, h: u16, scale: Option<f64>) -> Vec<String> {
    let mut args = vec![
        "--output".to_string(),
        output.to_string(),
        "--custom-mode".to_string(),
        format!("{w}x{h}"),
    ];
    if let Some(s) = scale {
        args.push("--scale".to_string());
        args.push(format!("{s}"));
    }
    args
}

/// 跑 wlr-randr 並回收 (stdout, stderr, 成功與否)。
///
/// **不能用 `Command::output()`**：supervisor 以 `waitpid(ANY)` 監督服務子行程，會把
/// 本執行緒 spawn 的 wlr-randr 也搶先收屍——`output()` 內部的 wait 便得 **ECHILD**
/// 而誤報失敗（實機 e2e 抓到：模式已套用、卻記為 failed）。改為自己讀完 stdout/stderr
/// （EOF = 子行程已結束），wait 遇 ECHILD 時以「stderr 是否為空」判定成敗——wlr-randr
/// 失敗必在 stderr 留話（協定缺失、參數錯等），成功則安靜。
fn run_wlr_randr(args: &[&str]) -> Result<(String, String, bool), String> {
    use std::io::Read as _;
    let mut child = Command::new("wlr-randr")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("wlr-randr: {e}"))?;
    // 輸出量極小（遠低於 pipe 緩衝），先讀完 stdout 再讀 stderr 不會互相塞住。
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    let ok = match child.wait() {
        Ok(status) => status.success(),
        // ECHILD：supervisor 已代收——子行程確定跑完（streams 已 EOF），以 stderr 判定。
        Err(e) if e.raw_os_error() == Some(libc::ECHILD) => stderr.trim().is_empty(),
        Err(e) => return Err(format!("wlr-randr wait: {e}")),
    };
    Ok((stdout, stderr, ok))
}

/// 從 `wlr-randr` 列表輸出取第一個 output 名（頂格行的第一個 token；
/// 縮排行是該 output 的屬性）。
fn first_output_name(list: &str) -> Option<String> {
    list.lines()
        .find(|l| !l.is_empty() && !l.starts_with(char::is_whitespace))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_kernel_abi() {
        // 已知常數（linux/drm.h 展開值）——同時守住 struct 大小/佈局不被改壞。
        assert_eq!(DRM_IOCTL_MODE_GETRESOURCES, 0xC040_64A0);
        assert_eq!(DRM_IOCTL_MODE_GETCONNECTOR, 0xC050_64A7);
        assert_eq!(std::mem::size_of::<DrmModeModeinfo>(), 68);
    }

    #[test]
    fn uevent_classify() {
        // kernel 原生格式：header\0KEY=VAL\0…
        let drm_change =
            b"change@/devices/pci0000:00/0000:00:01.0/virtio0/drm/card0\0ACTION=change\0DEVPATH=/devices/pci0000:00/0000:00:01.0/virtio0/drm/card0\0SUBSYSTEM=drm\0HOTPLUG=1\0DEVNAME=dri/card0\0";
        assert!(is_drm_change(drm_change));
        // 其他 subsystem 的 change 不觸發。
        assert!(!is_drm_change(
            b"change@/devices/virtual/block/loop0\0ACTION=change\0SUBSYSTEM=block\0"
        ));
        // drm 的 add（開機初始化）不觸發——cage 啟動時自己會拿 preferred mode。
        assert!(!is_drm_change(
            b"add@/devices/pci0000:00/0000:00:01.0/virtio0/drm/card0\0ACTION=add\0SUBSYSTEM=drm\0"
        ));
        // udevd group 2 轉播的 libudev magic header 不匹配。
        assert!(!is_drm_change(b"libudev\0\0\0\x01\0SUBSYSTEM=drm\0"));
        assert!(!is_drm_change(b""));
    }

    #[test]
    fn preferred_mode_wins_else_first() {
        let m = |w: u16, h: u16, typ: u32| DrmModeModeinfo {
            hdisplay: w,
            vdisplay: h,
            typ,
            ..Default::default()
        };
        // preferred 不在第一個也要選中。
        assert_eq!(
            pick_mode(&[
                m(1280, 800, 0),
                m(1024, 845, DRM_MODE_TYPE_PREFERRED),
                m(640, 480, 0)
            ]),
            Some((1024, 845))
        );
        // 無 preferred → 第一個。
        assert_eq!(
            pick_mode(&[m(800, 600, 0), m(640, 480, 0)]),
            Some((800, 600))
        );
        assert_eq!(pick_mode(&[]), None);
        // 零尺寸（two-call 競態下沒被複製到的殘留）不採——寧可退第一個可用者或 None。
        assert_eq!(
            pick_mode(&[m(0, 0, DRM_MODE_TYPE_PREFERRED), m(1280, 800, 0)]),
            Some((1280, 800))
        );
        assert_eq!(pick_mode(&[m(0, 0, 0)]), None);
    }

    #[test]
    fn gui_scale_parse() {
        // 標準 Retina（2×）與非整數 scale。
        assert_eq!(parse_gui_scale("quiet chefer.gui_scale=2 ro"), Some(2.0));
        assert_eq!(parse_gui_scale("chefer.gui_scale=1.5"), Some(1.5));
        // 1 = 不需縮放 → None（走與未附參數完全相同的路徑）。
        assert_eq!(parse_gui_scale("chefer.gui_scale=1"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=1.0"), None);
        // 無參數 / 空值 / 垃圾 → None。
        assert_eq!(parse_gui_scale("quiet ro"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale="), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=abc"), None);
        // 會毀版面的值一律不採：0、負、NaN、inf、超過 4。
        assert_eq!(parse_gui_scale("chefer.gui_scale=0"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=-2"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=nan"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=inf"), None);
        assert_eq!(parse_gui_scale("chefer.gui_scale=8"), None);
        // 與其他 chefer.* cmdline 參數共存。
        assert_eq!(
            parse_gui_scale("chefer.clip_token=abc chefer.gui_scale=2 chefer.clip_port=40000"),
            Some(2.0)
        );
    }

    #[test]
    fn mode_args_scale_flag() {
        // 無 scale：與既往完全相同的四個引數（WHP / vz 預設模式不受影響）。
        assert_eq!(
            mode_args("Virtual-1", 2200, 1400, None),
            ["--output", "Virtual-1", "--custom-mode", "2200x1400"]
        );
        // 有 scale：同一次 apply 一併帶 --scale（模式與 scale 同 commit）。
        assert_eq!(
            mode_args("Virtual-1", 2200, 1400, Some(2.0)),
            [
                "--output",
                "Virtual-1",
                "--custom-mode",
                "2200x1400",
                "--scale",
                "2"
            ]
        );
        // 非整數 scale 維持小數表示（wlr-randr 以 C locale 解析，小數點必為 '.'）。
        assert_eq!(mode_args("X", 100, 50, Some(1.5))[5], "1.5");
    }

    #[test]
    fn output_name_from_wlr_randr_listing() {
        let listing = "Virtual-1 \"Red Hat, Inc. (Virtual-1)\"\n  Physical size: 0x0 mm\n  Enabled: yes\n  Modes:\n    1280x800 px, 60.000000 Hz (preferred, current)\n";
        assert_eq!(first_output_name(listing).as_deref(), Some("Virtual-1"));
        // 空輸出（協定沒實作時 wlr-randr 直接錯誤，但守住 None 路徑）。
        assert_eq!(first_output_name(""), None);
        assert_eq!(first_output_name("  indented only\n"), None);
    }
}
