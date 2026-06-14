//! per-app network namespace（`internal` / `bridge` 模式）。
//!
//! 對應 docs/DESIGN.md「網路隔離」節。核心做法：
//!
//! - **一個 app netns（只有 `lo`）**由一個常駐的「holder 行程」持有：holder
//!   `unshare(NEWNET[|NEWUSER])`、把 `lo` 拉起、然後 `pause()` 不動，純粹讓 netns
//!   存活並可被 `setns` 加入。`/proc/<holder>/ns/net` 即 app netns 的 fd 來源。
//! - **每個服務**在 fork 出的中繼行程裡 `setns` 進這個 app netns（rootless 另先
//!   `setns` 進 holder 的 user ns 以取得 caps），再 `unshare(MNT|PID)`——見 `exec.rs`。
//! - **宣告的 inbound port** 由 supervisor 在 **parent netns** 開 listener，轉進
//!   app netns 的 `127.0.0.1:guest`。關鍵：**netns 是 per-thread**，故同一行程內可以
//!   「listener 執行緒留在 parent netns、另一條 factory 執行緒 `setns` 進 app netns
//!   負責建立 upstream socket」，fd 一旦建立即與 netns 無關，可跨執行緒讀寫——
//!   不需要 veth / iptables，也不需要跨行程 SCM_RIGHTS 傳 fd（見 `relay` 模組）。
//!
//! 權限模型：rootless 時 holder 以自身 uid 為 owner 建 user+net ns；supervisor
//! 行程（在祖先 user ns、euid == owner）對該 netns 持有 CAP_SYS_ADMIN，故 relay
//! factory 執行緒能直接 `setns(NEWNET)` 而**不必**也加入 user ns（保留 host 身分）。
//!
//! 本模組於 `lib.rs` 以 `#[cfg(target_os = "linux")]` 限定。

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork};

pub mod relay;

/// 已建立的 app netns：持有 holder pid 與其 ns fd。
pub struct AppNet {
    holder: Pid,
    /// `/proc/<holder>/ns/net`。
    net_fd: OwnedFd,
    /// `/proc/<holder>/ns/user`（僅 rootless）。
    user_fd: Option<OwnedFd>,
    rootless: bool,
    /// bridge 模式下提供出網 NAT 的 pasta 子行程（找不到 pasta 時為 None → 退化成 internal）。
    pasta: Option<Child>,
}

/// 傳給服務中繼行程、用以 `setns` 進 app netns 的 raw fd 集合。
/// fd 經 fork 繼承（CLOEXEC 設在 grandchild execve 才關閉，中繼行程使用期間仍有效）。
#[derive(Clone, Copy)]
pub struct NetnsJoin {
    pub net: RawFd,
    /// rootless 時為 holder user ns 的 fd；root 為 None。
    pub user: Option<RawFd>,
}

impl AppNet {
    /// 取得供服務中繼行程 `setns` 用的 raw fd。
    pub fn join(&self) -> NetnsJoin {
        NetnsJoin {
            net: self.net_fd.as_raw_fd(),
            user: self.user_fd.as_ref().map(|f| f.as_raw_fd()),
        }
    }

    /// app netns 的 net ns fd（供 relay factory `setns` 用）。
    pub fn net_raw(&self) -> RawFd {
        self.net_fd.as_raw_fd()
    }

    pub fn rootless(&self) -> bool {
        self.rootless
    }

    /// 收掉 holder 行程（app 結束時呼叫）。服務已各自退出後，netns 由 holder 維持，
    /// 殺掉 holder 即釋放。
    pub fn shutdown(mut self) {
        // 先收 pasta（出網橋）；它本會在 netns 消失後自行結束，這裡主動關閉更乾淨。
        if let Some(mut p) = self.pasta.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        let _ = kill(self.holder, Signal::SIGKILL);
        // 回收殭屍（避免留下 defunct）。
        loop {
            match waitpid(self.holder, Some(WaitPidFlag::empty())) {
                Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => break,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                _ => break,
            }
        }
    }
}

/// 建立 app netns：fork holder、`unshare` 出新 netns、拉起 `lo`，回傳可加入該 netns 的控制把手。
///
/// `rootless`：非 root 時 holder 另 `unshare(NEWUSER)` 並寫單一 uid/gid map（同
/// rootless 服務的身分模型），如此 holder 在自己的 user ns 內為 root，得以拉起 `lo`。
///
/// `bridge`：為 true 時額外嘗試啟動 bundled `pasta` 對該 netns 提供出網 NAT
/// （= Docker bridge 等價）。找不到 pasta 或啟動失敗時**不報錯**——退化成 internal
/// （只有 lo、無對外網路），並印出明確訊息。
/// `pasta_hint`：bundle 內嵌 pasta 的路徑（`agents/pasta-<arch>`），優先於 PATH。
pub fn setup_app_netns(
    rootless: bool,
    bridge: bool,
    pasta_hint: Option<PathBuf>,
) -> Result<AppNet> {
    // 就緒/錯誤回報管線：holder 設定完成後寫 1 byte（1=ok / 0=fail）。
    let (rd, wr) =
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).context("建立 netns 就緒管線失敗")?;

    // SAFETY: fork 後 holder 子行程僅呼叫 async-signal 相對安全的系統呼叫，最終 pause()。
    match unsafe { fork() }.context("fork netns holder 失敗")? {
        ForkResult::Child => {
            drop(rd);
            holder_main(rootless, wr) // 不返回（pause 迴圈）
        }
        ForkResult::Parent { child } => {
            drop(wr);
            // 等 holder 完成 lo 設定。
            let mut byte = [0u8; 1];
            let ok = {
                let mut f = std::fs::File::from(rd);
                matches!(f.read_exact(&mut byte), Ok(())) && byte[0] == 1
            };
            if !ok {
                let _ = kill(child, Signal::SIGKILL);
                let _ = waitpid(child, None);
                bail!(
                    "建立 app network namespace 失敗（holder 行程未就緒）；\
                     請確認核心允許 {}（rootless 需 unprivileged user namespaces）",
                    if rootless {
                        "unprivileged user+net namespaces"
                    } else {
                        "network namespaces"
                    }
                );
            }
            // 開啟 holder 的 ns fd（供服務與 relay factory setns）。
            let net_fd = open_ns_fd(child, "net").inspect_err(|_| {
                let _ = kill(child, Signal::SIGKILL);
            })?;
            let user_fd = if rootless {
                Some(open_ns_fd(child, "user").inspect_err(|_| {
                    let _ = kill(child, Signal::SIGKILL);
                })?)
            } else {
                None
            };
            // bridge：嘗試起 pasta 提供出網（失敗則退化為 internal，不致命）。
            let pasta = if bridge {
                start_pasta(child, rootless, pasta_hint)
            } else {
                None
            };
            Ok(AppNet {
                holder: child,
                net_fd,
                user_fd,
                rootless,
                pasta,
            })
        }
    }
}

/// bridge 模式：對 holder 的 netns 啟動 pasta（passt 的 netns 模式）提供出網 NAT。
/// 從 host netns（guest-agent 所在）執行，以 `--netns`/`--userns` 指向 holder 的 ns。
/// 找不到 pasta 或啟動失敗回 None（呼叫端據此退化為 internal）。
fn start_pasta(holder: Pid, rootless: bool, hint: Option<PathBuf>) -> Option<Child> {
    let prog = locate_pasta(hint);
    let net_ns = format!("/proc/{}/ns/net", holder.as_raw());
    let mut cmd = Command::new(&prog);
    // --config-net：在目標 netns 內自動配置位址/路由/DNS；--foreground：由我們掌控其生命週期。
    cmd.arg("--config-net").arg("--foreground");
    cmd.arg("--netns").arg(&net_ns);
    if rootless {
        cmd.arg("--userns")
            .arg(format!("/proc/{}/ns/user", holder.as_raw()));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(child) => {
            eprintln!(
                "[guest-agent] bridge 出網：已啟動 pasta（{}）對 app netns 提供 NAT",
                prog.to_string_lossy()
            );
            Some(child)
        }
        Err(e) => {
            eprintln!(
                "[guest-agent] bridge 模式找不到/無法啟動 pasta（{}：{e}）→ 退化為 internal\
                 （只有 lo、無對外網路）。請在 kit 內附上 pasta，或設定 CHEFER_PASTA 指向其路徑。",
                prog.to_string_lossy()
            );
            None
        }
    }
}

/// 尋找 pasta 執行檔，依序：
/// `CHEFER_PASTA` > bundle 內嵌（`agents/pasta-<arch>`）> guest-agent 同目錄/`kit/` > PATH 的 `pasta`。
fn locate_pasta(hint: Option<PathBuf>) -> OsString {
    if let Some(p) = std::env::var_os("CHEFER_PASTA")
        && !p.is_empty()
    {
        return p;
    }
    if let Some(h) = hint
        && h.is_file()
    {
        return h.into_os_string();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for cand in [dir.join("pasta"), dir.join("kit").join("pasta")] {
            if cand.is_file() {
                return cand.into_os_string();
            }
        }
    }
    // 最後退路：交給 PATH（spawn 失敗會在 start_pasta 退化處理）。
    OsString::from("pasta")
}

/// 開啟 `/proc/<pid>/ns/<which>` 為 CLOEXEC fd。
fn open_ns_fd(pid: Pid, which: &str) -> Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let path = format!("/proc/{}/ns/{}\0", pid.as_raw(), which);
    // O_RDONLY | O_CLOEXEC；ns fd 不需可寫。
    let raw = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("開啟 {} 失敗", &path[..path.len() - 1]));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// holder 子行程：unshare 出 netns（rootless 另含 user ns + uid map），拉起 lo，
/// 回報就緒後 pause() 常駐。`wr` 為就緒回報管線寫入端。
fn holder_main(rootless: bool, wr: OwnedFd) -> ! {
    // 父行程（supervisor）若先死，holder 連帶被 SIGKILL，避免遺留 netns。
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        )
    };

    let report = |ok: bool| {
        let b = [u8::from(ok)];
        let _ = nix::unistd::write(&wr, &b);
    };

    // rootless：**先單獨** unshare user ns 再寫單行 uid/gid map。
    // 不可把 NEWUSER 與 NEWNET 併在同一個 unshare()——某些核心在「同一呼叫同時建
    // user ns 與其他 ns」時，會拒絕之後的非特權單行 uid_map 寫入（EPERM）。
    // 分兩步（同 `unshare -U` → 寫 map → 再建 netns 的慣用法）即可正常。
    if rootless {
        if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
            eprintln!("[guest-agent] netns holder unshare(user) 失敗：{e}");
            report(false);
            unsafe { libc::_exit(1) }
        }
        if let Err(e) = write_self_id_maps() {
            eprintln!("[guest-agent] netns holder 寫入 uid/gid map 失敗：{e:#}");
            report(false);
            unsafe { libc::_exit(1) }
        }
    }
    // 此時（rootless 下已是 user ns 內 root，持 CAP_NET_ADMIN）再建 net ns。
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNET) {
        eprintln!("[guest-agent] netns holder unshare(net) 失敗：{e}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    if let Err(e) = bring_lo_up() {
        eprintln!("[guest-agent] netns holder 拉起 lo 失敗：{e:#}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    report(true);
    drop(wr);
    // 常駐：netns 由本行程維持存活，直到被 supervisor SIGKILL。
    loop {
        unsafe { libc::pause() };
    }
}

/// 在新建的 user ns 內把 uid/gid 0 映射到呼叫者自身（單一映射；同 exec.rs 的 rootless 模型）。
fn write_self_id_maps() -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    match std::fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("寫入 /proc/self/setgroups 失敗"),
    }
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1"))
        .context("寫入 holder uid_map 失敗")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1"))
        .context("寫入 holder gid_map 失敗")?;
    Ok(())
}

/// 在「當前 netns」內以 ioctl 把 loopback 介面拉起（SIOCGIFFLAGS → OR IFF_UP → SIOCSIFFLAGS）。
fn bring_lo_up() -> Result<()> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if sock < 0 {
        return Err(io::Error::last_os_error()).context("建立 ioctl socket 失敗");
    }
    let result = (|| -> Result<()> {
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        // ifr_name = "lo"
        let name = b"lo";
        for (i, b) in name.iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) } < 0 {
            return Err(io::Error::last_os_error()).context("SIOCGIFFLAGS(lo) 失敗");
        }
        // SAFETY: 讀取 ifreq 的 flags union 欄位（寫入 union 欄位本身不需 unsafe）。
        // 只設 IFF_UP（IFF_RUNNING 由核心管理，手動設可能被拒）。
        let cur = unsafe { ifr.ifr_ifru.ifru_flags };
        ifr.ifr_ifru.ifru_flags = cur | (libc::IFF_UP as i16);
        if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
            return Err(io::Error::last_os_error()).context("SIOCSIFFLAGS(lo, UP) 失敗");
        }
        Ok(())
    })();
    unsafe { libc::close(sock) };
    result
}

/// 讓「當前執行緒」加入 app netns（供 relay factory 使用）。只動 netns，不動 user ns
/// （supervisor 對該 netns 已持 CAP_SYS_ADMIN）。
pub fn enter_netns(net_fd: RawFd) -> Result<()> {
    let fd = unsafe { BorrowedFd::borrow_raw(net_fd) };
    setns(fd, CloneFlags::CLONE_NEWNET).context("relay factory setns(NEWNET) 失敗")?;
    Ok(())
}
