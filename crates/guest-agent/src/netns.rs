//! per-app network namespace（`internal` / `bridge` 模式）。
//!
//! 對應 docs/DESIGN.md「網路隔離」節。核心做法：
//!
//! - **一個 app netns（只有 `lo`）**由一個常駐的「holder 行程」持有：holder
//!   `unshare(NEWNET[|NEWUSER])`、把 `lo` 拉起、然後 `pause()` 不動，純粹讓 netns
//!   存活並可被 `setns` 加入。`/proc/<holder>/ns/net` 即 app netns 的 fd 來源。
//! - **每個服務**在 fork 出的中繼行程裡 `setns` 進這個 app netns（rootless 另先
//!   `setns` 進 holder 的 user ns 以取得 caps），再 `unshare(MNT|PID)`——見 `exec.rs`。
//! - **宣告的 inbound port** 由 supervisor 在 **parent netns** 開 listener，需把流量轉進
//!   app netns 的 `127.0.0.1:guest`。upstream socket 必須誕生在 app netns 內，但 rootless
//!   下 supervisor（在 init user ns、無 CAP_SYS_ADMIN）**無法** `setns(NEWNET)`
//!   ——`setns` 進 netns 需要「呼叫者當前 user ns」與「netns 擁有者 user ns」**兩邊**都有
//!   CAP_SYS_ADMIN；而執行緒又不能 `setns(NEWUSER)`（須單執行緒）。故由**已在 netns+userns
//!   內為 root 的 holder 行程當 socket factory**：supervisor 透過一條 socketpair 送出
//!   `(proto, port)` 請求，holder 建立連到 `127.0.0.1:guest` 的 socket，再以 **SCM_RIGHTS**
//!   把 fd 傳回 supervisor。fd 一旦建立即與 netns 無關，bidir 搬運留在 parent netns。
//!   ——不需要 veth / iptables / slirp。
//!
//! 本模組於 `lib.rs` 以 `#[cfg(target_os = "linux")]` 限定。

use std::ffi::OsString;
use std::io::{self, IoSlice, IoSliceMut, Read};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
    sendmsg, socketpair,
};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Gid, Pid, Uid, fork, setgid, setgroups, setuid};

pub mod relay;

const PROTO_TCP: u8 = 1;
const PROTO_UDP: u8 = 2;
/// netns holder 與 pasta 在 root 後端改以此非特權 uid/gid 執行（nobody）：pasta 以 root
/// 執行會自我停用、且無法存取 root 擁有的 ns；以非特權身分執行則與 rootless 路徑等效。
const NOBODY_ID: u32 = 65534;

/// 已建立的 app netns：持有 holder pid、其 ns fd，與通往 holder socket factory 的通道。
pub struct AppNet {
    holder: Pid,
    /// `/proc/<holder>/ns/net`。
    net_fd: OwnedFd,
    /// `/proc/<holder>/ns/user`（holder 一律有 user ns，owner = net_uid；供 pasta `--userns`）。
    user_fd: Option<OwnedFd>,
    rootless: bool,
    /// bridge 模式下提供出網 NAT 的 pasta 子行程（找不到 pasta 時為 None → 退化成 internal）。
    pasta: Option<Child>,
    /// 通往 holder socket factory 的 socketpair 端點（送 (proto,port)、收 SCM_RIGHTS fd）。
    factory: Arc<Mutex<OwnedFd>>,
}

/// 向 holder socket factory 請求「連到 app netns 內 `127.0.0.1:port` 的 socket」的把手。
/// 可 clone，供多個 relay listener 共用（同一 socketpair 以 Mutex 串行化請求/回應）。
#[derive(Clone)]
pub struct Dialer {
    sock: Arc<Mutex<OwnedFd>>,
}

impl Dialer {
    /// 取得一條（app netns 內）已連到 `127.0.0.1:port` 的 TCP socket。
    pub fn tcp(&self, port: u16) -> io::Result<TcpStream> {
        let fd = self.dial(PROTO_TCP, port)?;
        Ok(TcpStream::from(fd))
    }

    /// 取得一條（app netns 內）已 connect 到 `127.0.0.1:port` 的 UDP socket。
    pub fn udp(&self, port: u16) -> io::Result<UdpSocket> {
        let fd = self.dial(PROTO_UDP, port)?;
        Ok(UdpSocket::from(fd))
    }

    fn dial(&self, proto: u8, port: u16) -> io::Result<OwnedFd> {
        let guard = self
            .sock
            .lock()
            .map_err(|_| io::Error::other("factory channel poisoned"))?;
        let req = [proto, (port & 0xff) as u8, (port >> 8) as u8];
        // 請求：3-byte (proto, port_le)。
        nix::unistd::write(&*guard, &req).map_err(io::Error::from)?;
        // 回應：1-byte status（0=ok）+（成功時）SCM_RIGHTS 夾帶的 fd。
        let mut status = [0u8; 1];
        let mut iov = [IoSliceMut::new(&mut status)];
        let mut cmsg = nix::cmsg_space!([RawFd; 1]);
        let msg = recvmsg::<()>(
            guard.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::empty(),
        )
        .map_err(io::Error::from)?;
        let mut got: Option<RawFd> = None;
        for c in msg.cmsgs().map_err(io::Error::from)? {
            if let ControlMessageOwned::ScmRights(fds) = c {
                for f in fds {
                    if got.is_none() {
                        got = Some(f);
                    } else {
                        // SAFETY: 多餘 fd（不應發生）直接關閉避免洩漏。
                        let _ = unsafe { OwnedFd::from_raw_fd(f) };
                    }
                }
            }
        }
        match (status[0], got) {
            // SAFETY: fd 由 holder 經 SCM_RIGHTS 傳來，所有權移交給此 OwnedFd。
            (0, Some(fd)) => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
            (_, Some(fd)) => {
                let _ = unsafe { OwnedFd::from_raw_fd(fd) };
                Err(io::Error::other(
                    "holder reported failure creating upstream",
                ))
            }
            _ => Err(io::Error::other("holder did not return an upstream fd")),
        }
    }
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
    /// `user` 僅 rootless 時提供：rootless 服務需加入 holder user ns 取得 caps；root 後端的
    /// 服務只 `setns` 進 netns、**不**加入 user ns（保持真實 root → chown/gosu 可用）。
    pub fn join(&self) -> NetnsJoin {
        NetnsJoin {
            net: self.net_fd.as_raw_fd(),
            user: if self.rootless {
                self.user_fd.as_ref().map(|f| f.as_raw_fd())
            } else {
                None
            },
        }
    }

    /// 取得向 holder socket factory 請求 app-netns upstream 的把手。
    pub fn dialer(&self) -> Dialer {
        Dialer {
            sock: Arc::clone(&self.factory),
        }
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

/// 建立 app netns：fork holder、`unshare` 出新 user+net ns、拉起 `lo`，回傳控制把手。
///
/// holder **一律**建立 user ns（owner = net_uid：rootless 為呼叫者、root 後端為 nobody），
/// 使 netns 由非特權 user ns 擁有，bridge 的 pasta（同以 net_uid 執行）才 `--userns` 進得來。
/// rootless 服務會加入此 user ns 取得 caps；root 後端的服務只 `setns` 進 netns、保持真實 root。
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
    // holder 與 pasta 執行用的 uid/gid：rootless 沿用呼叫者自身；root 後端降到 nobody
    //（pasta 以 root 無法運作，且 root 擁有的 ns 在 pasta 降權後存取不到）。
    let (net_uid, net_gid) = if rootless {
        (
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        )
    } else {
        (NOBODY_ID, NOBODY_ID)
    };

    // 就緒/錯誤回報管線：holder 設定完成後寫 1 byte（1=ok / 0=fail）。
    let (rd, wr) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .context("failed to create netns readiness pipe")?;
    // socket factory 通道：SEQPACKET 保訊息邊界；parent 送 (proto,port)、holder 回 SCM_RIGHTS fd。
    let (sv_parent, sv_holder) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .context("failed to create netns factory socketpair")?;

    // SAFETY: fork 後 holder 子行程僅呼叫相對安全的系統呼叫；holder 為 fork-without-exec
    //（在尚未生成任何執行緒的早期階段建立，見 run_bundle_linux 的呼叫順序）。
    match unsafe { fork() }.context("failed to fork netns holder")? {
        ForkResult::Child => {
            drop(rd);
            drop(sv_parent);
            holder_main(net_uid, net_gid, wr, sv_holder) // 不返回（factory 迴圈 → pause）
        }
        ForkResult::Parent { child } => {
            drop(wr);
            drop(sv_holder);
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
                    "failed to create the app network namespace (holder process not ready); \
                     check that the kernel allows {} (rootless requires unprivileged user namespaces)",
                    if rootless {
                        "unprivileged user+net namespaces"
                    } else {
                        "network namespaces"
                    }
                );
            }
            // 開啟 holder 的 ns fd。holder 一律有 user ns（owner = net_uid）：net 供服務與
            // SCM_RIGHTS factory；user 供 pasta `--userns`。服務是否加入 user ns 由 join() 依
            // rootless 決定（root 後端的服務不加入 → 保持真實 root，chown/gosu 不受影響）。
            let net_fd = open_ns_fd(child, "net").inspect_err(|_| {
                let _ = kill(child, Signal::SIGKILL);
            })?;
            let user_fd = Some(open_ns_fd(child, "user").inspect_err(|_| {
                let _ = kill(child, Signal::SIGKILL);
            })?);
            // bridge：嘗試起 pasta（以 net_uid 執行）提供出網（失敗則退化為 internal，不致命）。
            let pasta = if bridge {
                start_pasta(child, net_uid, net_gid, pasta_hint)
            } else {
                None
            };
            Ok(AppNet {
                holder: child,
                net_fd,
                user_fd,
                rootless,
                pasta,
                factory: Arc::new(Mutex::new(sv_parent)),
            })
        }
    }
}

/// bridge 模式：對 holder 的 netns 啟動 pasta（passt 的 netns 模式）提供出網 NAT。
/// 從 host netns（guest-agent 所在）執行，以 `--netns`/`--userns` 指向 holder 的 ns。
/// 找不到 pasta 或啟動失敗回 None（呼叫端據此退化為 internal）。
fn start_pasta(holder: Pid, net_uid: u32, net_gid: u32, hint: Option<PathBuf>) -> Option<Child> {
    let prog = locate_pasta(hint);
    match spawn_pasta(&prog, holder, net_uid, net_gid) {
        Ok(child) => {
            eprintln!(
                "[guest-agent] bridge outbound: started pasta ({}) providing NAT for the app netns",
                prog.to_string_lossy()
            );
            Some(child)
        }
        // 在 Windows host 打包的 bundle 無法記錄 unix 執行位（pack 的 chmod 只在 unix 生效），
        // 解到 guest 後 pasta 是 0644 → exec EACCES。補救：複製到 tmp、補 0755 再重試一次
        // （同時涵蓋唯讀掛載的情境）。
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let staged = std::env::temp_dir().join(format!("chefer-pasta-{}", std::process::id()));
            let retried = std::fs::copy(&prog, &staged).and_then(|_| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
                spawn_pasta(staged.as_os_str(), holder, net_uid, net_gid)
            });
            match retried {
                Ok(child) => {
                    eprintln!(
                        "[guest-agent] bridge outbound: started pasta (staged copy of {}; original lacked the exec bit) providing NAT for the app netns",
                        prog.to_string_lossy()
                    );
                    Some(child)
                }
                Err(e2) => {
                    eprintln!(
                        "[guest-agent] bridge mode could not start pasta ({}: {e}; staged-copy retry: {e2}) → degrading to internal \
                         (only lo, no outbound network). Bundle pasta in the kit, or set CHEFER_PASTA to its path.",
                        prog.to_string_lossy()
                    );
                    None
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[guest-agent] bridge mode could not find/start pasta ({}: {e}) → degrading to internal\
                 (only lo, no outbound network). Bundle pasta in the kit, or set CHEFER_PASTA to its path.",
                prog.to_string_lossy()
            );
            None
        }
    }
}

/// 組裝並 spawn pasta 行程（start_pasta 的單次嘗試；EACCES 補救重試共用）。
fn spawn_pasta(
    prog: &std::ffi::OsStr,
    holder: Pid,
    net_uid: u32,
    net_gid: u32,
) -> io::Result<Child> {
    let net_ns = format!("/proc/{}/ns/net", holder.as_raw());
    let user_ns = format!("/proc/{}/ns/user", holder.as_raw());
    let mut cmd = Command::new(prog);
    // --config-net：在目標 netns 內自動配置位址/路由/DNS；--foreground：由我們掌控其生命週期。
    cmd.arg("--config-net").arg("--foreground");
    // 關閉 pasta 的「自動轉發所有 port」——否則它會在 netns 內搶先 bind 服務要用的 port
    // （服務隨後 bind 同 port → EADDRINUSE）。inbound 由我們自己的 relay 負責；pasta 僅需提供
    // app 主動發起的 outbound NAT（與這些 listening-port 轉發無關，仍正常運作）。
    cmd.arg("--tcp-ports").arg("none");
    cmd.arg("--udp-ports").arg("none");
    cmd.arg("--tcp-ns").arg("none");
    cmd.arg("--udp-ns").arg("none");
    cmd.arg("--netns").arg(&net_ns);
    // 一律帶 --userns（holder 一律有 user ns）：pasta 須先進入擁有此 netns 的 user ns 才存取得到。
    cmd.arg("--userns").arg(&user_ns);
    // 以 net_uid/gid 執行 pasta：pasta 以 root 會自我停用，必須非特權執行。net_uid 即 holder
    // user ns 的 owner（rootless=呼叫者；root 後端=nobody），故 pasta 進得了該 user ns。
    cmd.uid(net_uid).gid(net_gid);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
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
    let path = format!("/proc/{}/ns/{}\0", pid.as_raw(), which);
    // O_RDONLY | O_CLOEXEC；ns fd 不需可寫。
    let raw = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to open {}", &path[..path.len() - 1]));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// holder 子行程：unshare 出 netns（rootless 另含 user ns + uid map），拉起 lo，回報就緒後
/// 進入 socket factory 迴圈（為 supervisor 建立 app-netns 內的 upstream 並以 SCM_RIGHTS 回傳），
/// 通道關閉後 pause() 常駐以維持 netns 存活。`wr` 為就緒回報管線，`factory` 為 SCM_RIGHTS 通道。
fn holder_main(net_uid: u32, net_gid: u32, wr: OwnedFd, factory: OwnedFd) -> ! {
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

    // root 後端：先降權到非特權 net_uid（nobody）。如此 holder 的 user ns owner = nobody，
    // pasta（同樣以 nobody 執行）才進得了；服務則仍以真實 root 只 setns 進 netns（見 join()）。
    // 先 setgroups→setgid→setuid（setuid 後即失去改 gid/groups 的權限）。
    if nix::unistd::geteuid().as_raw() != net_uid {
        if let Err(e) = setgroups(&[]) {
            eprintln!("[guest-agent] netns holder setgroups failed: {e}");
            report(false);
            unsafe { libc::_exit(1) }
        }
        if setgid(Gid::from_raw(net_gid)).is_err() || setuid(Uid::from_raw(net_uid)).is_err() {
            eprintln!("[guest-agent] netns holder failed to drop privileges to nobody");
            report(false);
            unsafe { libc::_exit(1) }
        }
        // setuid 降權會清掉 dumpable 旗標 → /proc/self/* 變成 root 擁有、本行程寫不了
        // （setgroups/uid_map 會 EPERM）。重設 dumpable=1 恢復對自身 /proc 的寫入權。
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    }

    // **一律**先單獨 unshare user ns、寫單行 uid/gid map，再建 netns。
    // 即使最終服務以 root 執行，netns 也由 holder 自己的 user ns（owner = net_uid）擁有，
    // 好讓 bridge 的 pasta（以 net_uid 執行、會降權）`--userns` 進得來；root 服務不加入此 ns。
    // 此處 getuid() 已是 net_uid（root 後端已降權）。
    let real_uid = nix::unistd::getuid().as_raw();
    let real_gid = nix::unistd::getgid().as_raw();
    if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
        eprintln!("[guest-agent] netns holder unshare(user) failed: {e}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    if let Err(e) = write_self_id_maps(real_uid, real_gid) {
        eprintln!("[guest-agent] netns holder failed to write uid/gid map: {e:#}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    // 此時已是 user ns 內 root（持 CAP_NET_ADMIN），再建 net ns。
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNET) {
        eprintln!("[guest-agent] netns holder unshare(net) failed: {e}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    if let Err(e) = bring_lo_up() {
        eprintln!("[guest-agent] netns holder failed to bring up lo: {e:#}");
        report(false);
        unsafe { libc::_exit(1) }
    }
    report(true);
    drop(wr);
    // socket factory：服務 upstream 必須誕生在此（app netns 內）。處理請求直到通道關閉。
    factory_loop(&factory);
    drop(factory);
    // 通道關閉（supervisor 不再需要 relay，或正在收尾）：常駐維持 netns 存活直到被 SIGKILL。
    loop {
        unsafe { libc::pause() };
    }
}

/// holder 的 socket factory 迴圈（在 app netns 內執行）：
/// 收 3-byte 請求 `(proto, port_le)` → 建立連到 `127.0.0.1:port` 的 socket → 以 SCM_RIGHTS 回傳。
/// 通道 EOF/錯誤即返回（由呼叫端轉入 pause）。
fn factory_loop(sock: &OwnedFd) {
    loop {
        let mut req = [0u8; 3];
        match nix::unistd::read(sock, &mut req) {
            Ok(3) => {}
            _ => return, // EOF / 短讀 / 錯誤 → 結束 factory
        }
        let proto = req[0];
        let port = u16::from_le_bytes([req[1], req[2]]);
        match make_upstream(proto, port) {
            Ok(fd) => send_upstream_fd(sock, fd.as_raw_fd()),
            Err(_) => send_status(sock, 1),
        }
    }
}

/// 在當前（app）netns 內建立連到 `127.0.0.1:port` 的 socket。
fn make_upstream(proto: u8, port: u16) -> io::Result<OwnedFd> {
    match proto {
        PROTO_TCP => Ok(OwnedFd::from(TcpStream::connect((
            Ipv4Addr::LOCALHOST,
            port,
        ))?)),
        PROTO_UDP => {
            let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            s.connect((Ipv4Addr::LOCALHOST, port))?;
            Ok(OwnedFd::from(s))
        }
        _ => Err(io::Error::other("unknown proto")),
    }
}

/// 回傳成功狀態（status=0）＋夾帶的 upstream fd（SCM_RIGHTS）。
fn send_upstream_fd(sock: &OwnedFd, fd: RawFd) {
    let payload = [0u8];
    let iov = [IoSlice::new(&payload)];
    let fds = [fd];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    let _ = sendmsg::<()>(sock.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None);
}

/// 回傳失敗狀態（無 fd）。
fn send_status(sock: &OwnedFd, status: u8) {
    let payload = [status];
    let iov = [IoSlice::new(&payload)];
    let _ = sendmsg::<()>(sock.as_raw_fd(), &iov, &[], MsgFlags::empty(), None);
}

/// 在新建的 user ns 內把 uid/gid 0 映射到呼叫者自身（單一映射；同 exec.rs 的 rootless 模型）。
/// `uid`/`gid` 必須是 **unshare(NEWUSER) 之前**取得的真實 id（之後 getuid 會回 overflow id）。
fn write_self_id_maps(uid: u32, gid: u32) -> Result<()> {
    match std::fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("failed to write /proc/self/setgroups"),
    }
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1"))
        .context("failed to write holder uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1"))
        .context("failed to write holder gid_map")?;
    Ok(())
}

/// 在「當前 netns」內以 ioctl 把 loopback 介面拉起（SIOCGIFFLAGS → OR IFF_UP → SIOCSIFFLAGS）。
fn bring_lo_up() -> Result<()> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if sock < 0 {
        return Err(io::Error::last_os_error()).context("failed to create ioctl socket");
    }
    let result = (|| -> Result<()> {
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        // ifr_name = "lo"
        let name = b"lo";
        for (i, b) in name.iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) } < 0 {
            return Err(io::Error::last_os_error()).context("SIOCGIFFLAGS(lo) failed");
        }
        // SAFETY: 讀取 ifreq 的 flags union 欄位（寫入 union 欄位本身不需 unsafe）。
        // 只設 IFF_UP（IFF_RUNNING 由核心管理，手動設可能被拒）。
        let cur = unsafe { ifr.ifr_ifru.ifru_flags };
        ifr.ifr_ifru.ifru_flags = cur | (libc::IFF_UP as i16);
        if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
            return Err(io::Error::last_os_error()).context("SIOCSIFFLAGS(lo, UP) failed");
        }
        Ok(())
    })();
    unsafe { libc::close(sock) };
    result
}
