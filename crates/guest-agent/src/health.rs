//! 服務健康檢查：在服務的 namespaces 內執行 `healthcheck.test`（nsenter + chroot）。
//!
//! 見 docs/DESIGN.md「健康檢查」。supervisor 追蹤每個服務的 pid-ns init host pid
//! （`init_pid`），本模組據此進入該服務的 user(僅 rootless)/mnt/net/pid namespaces，
//! 並 chroot 進 `/proc/<init>/root`（容器 rootfs），再 exec test；exit 0 = healthy。
//!
//! 為何能進 namespace（rootless 也行）：探測由 supervisor **fork 出的單執行緒行程**發起，
//! 其 euid == 服務 user ns 的擁有者 → 對該 user ns 具 CAP_SYS_ADMIN，故可 `setns`
//! （這點與 inbound relay 不同——relay 跑在多執行緒裡、不能 `setns(NEWUSER)`，才需 holder）。
//! 本模組僅於 Linux 編譯（見 `lib.rs` 的 cfg）。

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result, bail};
use nix::unistd::{ForkResult, Pid, fork};

use chefer_bundle::{CmdSpec, HealthCheck};

/// 容器內預設 PATH（用於解析無斜線的 test 命令）。
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// runner 內部回報用的退出碼（皆視為「本次探測失敗」）。
const EXIT_NSENTER_FAIL: i32 = 125; // setns/fork/chroot 失敗
const EXIT_TIMEOUT: i32 = 124; // test 超過 timeout
const EXIT_EXEC_FAIL: i32 = 127; // execve 失敗（命令不存在等）

/// 一次健康探測的結果。
pub enum Probe {
    /// test exit 0。
    Healthy,
    /// 跑完但失敗，帶可區分的原因（供記錄成可行動訊息）。
    Failed(ProbeFail),
}

/// 探測失敗的原因（對應 runner 的退出碼，沿用 shell 慣例：124 逾時、127 找不到命令）。
#[derive(Debug, Clone, Copy)]
pub enum ProbeFail {
    /// test 命令本身以非零碼結束。
    NonZero(i32),
    /// test 超過 timeout。
    Timeout,
    /// test 命令在 image 內找不到 / 無法 exec。
    ExecNotFound,
    /// 進不了服務的 namespace（多半是服務已退出）。
    NsenterFailed,
}

impl ProbeFail {
    /// 給使用者看的英文說明。
    pub fn describe(self) -> String {
        match self {
            ProbeFail::NonZero(c) => format!("test command exited with status {c}"),
            ProbeFail::Timeout => "test command timed out".to_string(),
            ProbeFail::ExecNotFound => {
                "test command not found in the image (check healthcheck.test)".to_string()
            }
            ProbeFail::NsenterFailed => {
                "could not run the probe inside the service (it may have already exited)"
                    .to_string()
            }
        }
    }
}

fn classify(exit_code: i32) -> Probe {
    match exit_code {
        0 => Probe::Healthy,
        EXIT_TIMEOUT => Probe::Failed(ProbeFail::Timeout),
        EXIT_NSENTER_FAIL => Probe::Failed(ProbeFail::NsenterFailed),
        EXIT_EXEC_FAIL => Probe::Failed(ProbeFail::ExecNotFound),
        c => Probe::Failed(ProbeFail::NonZero(c)),
    }
}

/// 進行一次健康探測：在服務（`init`）的 namespaces 內 exec test。
///
/// 回傳 `Ok(Probe::Healthy)` = test exit 0；`Ok(Probe::Failed(reason))` = 跑完但失敗
/// （帶原因）；`Err` = 連探測行程都 fork 不出來（呼叫端視為一次失敗）。
pub fn probe(init: Pid, hc: &HealthCheck) -> Result<Probe> {
    // fork 前先備妥 exec 計畫與所有 fd（child 內不再配置記憶體 / 開檔）。
    let prep = ExecPrep::prepare(init, hc)?;
    let base = format!("/proc/{}", init.as_raw());
    let root = open_path_dir(&format!("{base}/root"))
        .with_context(|| format!("open {base}/root failed (service may have exited)"))?;
    let mnt = open_cloexec(&format!("{base}/ns/mnt"))?;
    let net = open_cloexec(&format!("{base}/ns/net"))?;
    let pid = open_cloexec(&format!("{base}/ns/pid"))?;
    // root 後端服務與 supervisor 同 user ns → None（不需也不必 setns(NEWUSER)）。
    let user = open_user_ns_if_distinct(&base)?;

    let timeout_ms = hc.timeout_ms.max(1);

    // SAFETY: fork 後 child 僅呼叫 async-signal-safe 的系統呼叫，且失敗一律 `_exit`。
    match unsafe { fork() }.context("fork health-check runner failed")? {
        ForkResult::Parent { child } => match nix::sys::wait::waitpid(child, None) {
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => Ok(classify(code)),
            // runner 被訊號終止：當作進 ns 階段的失敗。
            Ok(_) => Ok(Probe::Failed(ProbeFail::NsenterFailed)),
            Err(e) => bail!("waiting for health-check runner failed: {e}"),
        },
        ForkResult::Child => {
            let code = runner(user, mnt, net, pid, root, &prep, timeout_ms);
            unsafe { libc::_exit(code) };
        }
    }
}

/// runner：進入服務 namespaces → 再 fork（孫行程成為 pid-ns 成員）→ chroot → exec test。
/// 回傳值即本探測行程的退出碼（0 = healthy）。
fn runner(
    user: Option<OwnedFd>,
    mnt: OwnedFd,
    net: OwnedFd,
    pid: OwnedFd,
    root: OwnedFd,
    prep: &ExecPrep,
    timeout_ms: u64,
) -> i32 {
    // user ns 須先進（取得該 ns 內 root 與 CAP_SYS_ADMIN，才能 setns 其餘）。
    if let Some(u) = &user
        && unsafe { libc::setns(u.as_raw_fd(), libc::CLONE_NEWUSER) } != 0
    {
        return EXIT_NSENTER_FAIL;
    }
    if unsafe { libc::setns(mnt.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
        return EXIT_NSENTER_FAIL;
    }
    if unsafe { libc::setns(net.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
        return EXIT_NSENTER_FAIL;
    }
    // CLONE_NEWPID 只影響其後 fork 的子行程，故 setns 後需再 fork。
    if unsafe { libc::setns(pid.as_raw_fd(), libc::CLONE_NEWPID) } != 0 {
        return EXIT_NSENTER_FAIL;
    }

    match unsafe { libc::fork() } {
        -1 => EXIT_NSENTER_FAIL,
        0 => {
            // 孫行程：chroot 進容器 rootfs 後 exec test。
            unsafe {
                if libc::fchdir(root.as_raw_fd()) != 0 {
                    libc::_exit(EXIT_NSENTER_FAIL);
                }
                if libc::chroot(c".".as_ptr()) != 0 {
                    libc::_exit(EXIT_NSENTER_FAIL);
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    libc::_exit(EXIT_NSENTER_FAIL);
                }
                libc::execve(
                    prep.program.as_ptr(),
                    prep.argv_ptr.as_ptr(),
                    prep.envp_ptr.as_ptr(),
                );
                libc::_exit(EXIT_EXEC_FAIL);
            }
        }
        child => wait_child_with_timeout(child, timeout_ms),
    }
}

/// 等待 exec 子行程，最多 `timeout_ms`；逾時 SIGKILL 並回 [`EXIT_TIMEOUT`]。
fn wait_child_with_timeout(child: libc::pid_t, timeout_ms: u64) -> i32 {
    const STEP_MS: u64 = 20;
    let mut waited = 0u64;
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if r == child {
            if libc::WIFEXITED(status) {
                return libc::WEXITSTATUS(status);
            }
            return 128 + libc::WTERMSIG(status);
        }
        if r < 0 {
            return EXIT_NSENTER_FAIL;
        }
        if waited >= timeout_ms {
            unsafe {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
            }
            return EXIT_TIMEOUT;
        }
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: (STEP_MS * 1_000_000) as _,
        };
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
        waited += STEP_MS;
    }
}

/// fork 前備妥的 exec 計畫（CString 與其指標陣列；child 直接使用，不再配置）。
struct ExecPrep {
    program: CString,
    // 持有 CString 以維持 *const 指標有效（argv_ptr / envp_ptr 指向其內部）。
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptr: Vec<*const libc::c_char>,
    envp_ptr: Vec<*const libc::c_char>,
}

impl ExecPrep {
    fn prepare(init: Pid, hc: &HealthCheck) -> Result<ExecPrep> {
        let (program, argv_strs): (String, Vec<String>) = match &hc.test {
            CmdSpec::Shell(s) => (
                "/bin/sh".to_string(),
                vec!["/bin/sh".to_string(), "-c".to_string(), s.clone()],
            ),
            CmdSpec::Argv(v) => {
                let prog = v.first().cloned().unwrap_or_default();
                (resolve_program(init, &prog), v.clone())
            }
        };
        let program = CString::new(program).context("health-check command contains NUL")?;
        let argv: Vec<CString> = argv_strs
            .into_iter()
            .map(CString::new)
            .collect::<std::result::Result<_, _>>()
            .context("health-check argv contains NUL")?;
        let envp: Vec<CString> = vec![CString::new(format!("PATH={DEFAULT_PATH}")).unwrap()];

        let mut argv_ptr: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptr.push(std::ptr::null());
        let mut envp_ptr: Vec<*const libc::c_char> = envp.iter().map(|c| c.as_ptr()).collect();
        envp_ptr.push(std::ptr::null());

        Ok(ExecPrep {
            program,
            _argv: argv,
            _envp: envp,
            argv_ptr,
            envp_ptr,
        })
    }
}

/// 解析無斜線的命令：在容器 rootfs（經 `/proc/<init>/root`）的 PATH 各目錄找；
/// 找到回傳容器內絕對路徑，否則原樣回傳（交給 execve 失敗 → 視為不健康）。
fn resolve_program(init: Pid, prog: &str) -> String {
    if prog.contains('/') {
        return prog.to_string();
    }
    let root = format!("/proc/{}/root", init.as_raw());
    for dir in DEFAULT_PATH.split(':') {
        if std::path::Path::new(&format!("{root}{dir}/{prog}")).exists() {
            return format!("{dir}/{prog}");
        }
    }
    prog.to_string()
}

/// 開 `/proc/.../root` 為 O_PATH 目錄 fd（供 fchdir+chroot）。
fn open_path_dir(path: &str) -> Result<OwnedFd> {
    let c = CString::new(path).context("path contains NUL")?;
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path} failed"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// 開 ns fd（O_RDONLY | O_CLOEXEC）。
fn open_cloexec(path: &str) -> Result<OwnedFd> {
    let c = CString::new(path).context("path contains NUL")?;
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path} failed"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// 服務 user ns 與自身相同 → None（root 後端）；不同 → Some(fd)（rootless）。
fn open_user_ns_if_distinct(base: &str) -> Result<Option<OwnedFd>> {
    let svc = open_cloexec(&format!("{base}/ns/user"))?;
    let me = open_cloexec("/proc/self/ns/user")?;
    if same_inode(&svc, &me)? {
        Ok(None)
    } else {
        Ok(Some(svc))
    }
}

fn same_inode(a: &OwnedFd, b: &OwnedFd) -> Result<bool> {
    let sa = fstat(a)?;
    let sb = fstat(b)?;
    Ok(sa.st_dev == sb.st_dev && sa.st_ino == sb.st_ino)
}

fn fstat(fd: &OwnedFd) -> Result<libc::stat> {
    // SAFETY: 傳入有效 fd 與已配置的 stat buffer。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstat failed");
    }
    Ok(st)
}
