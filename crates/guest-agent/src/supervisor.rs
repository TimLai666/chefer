//! 服務生命週期監控（fail_fast）：
//! 依拓撲順序啟動全部服務 → 任一服務以非零結束 → SIGTERM 其餘 → 5 秒後 SIGKILL
//! → 回傳該 exit code；全部正常結束 → 0；收到 SIGTERM/SIGINT → 同終止程序 → 130。
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, killpg, sigaction};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::exec::{SpawnSpec, spawn_service};

/// 一個執行中服務：中繼行程 pid（= pgid）、服務本體（pid-ns init）host pid、名稱。
struct Running {
    /// 中繼行程 pid，亦為其 process group id（供 killpg）。
    mid: Pid,
    /// 服務本體在 host 的 pid（pid-ns 的 init）；用於直接終止，
    /// 不受服務 setsid/setpgid 脫離中繼 process group 的影響。
    init: Option<Pid>,
    name: String,
}

/// SIGTERM / SIGINT 旗標（由訊號處理常式設定，主迴圈輪詢）。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_shutdown_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 安裝 SIGTERM / SIGINT 處理（不設 SA_RESTART，讓 waitpid 以 EINTR 醒來）。
/// 應在 rootfs 組裝前就安裝，使整個執行期間的終止訊號都走相同流程（回 130）。
pub fn install_signal_handlers() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(on_shutdown_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: handler 僅寫入 AtomicBool，為 async-signal-safe。
    unsafe {
        sigaction(Signal::SIGTERM, &action).context("安裝 SIGTERM 處理失敗")?;
        sigaction(Signal::SIGINT, &action).context("安裝 SIGINT 處理失敗")?;
    }
    Ok(())
}

/// 依拓撲順序啟動所有服務並監控至結束，回傳 app 整體 exit code。
///
/// v1 啟動順序即依賴語意（無健康檢查）：depends_on 只保證「先 spawn」，
/// 不等待被依賴服務就緒。
pub fn run_services(
    order: &[&chefer_bundle::ServiceEntry],
    rootfs_map: &BTreeMap<String, PathBuf>,
    data_dir: &Path,
) -> Result<i32> {
    // 啟動（中途收到關閉訊號 → 終止已啟動者並回 130）
    let mut running: Vec<Running> = Vec::new();
    for svc in order {
        if SHUTDOWN.load(Ordering::SeqCst) {
            terminate_all(&mut running);
            return Ok(130);
        }
        let rootfs = rootfs_map
            .get(&svc.name)
            .ok_or_else(|| anyhow::anyhow!("內部錯誤：服務 `{}` 缺少 rootfs 對應", svc.name))?;
        let terminal = svc.interface_mode.wants_terminal();
        let spawned = spawn_service(&SpawnSpec {
            service: svc,
            rootfs,
            data_dir,
            terminal,
        })
        .with_context(|| format!("啟動服務 `{}` 失敗", svc.name));
        let spawned = match spawned {
            Ok(s) => s,
            Err(e) => {
                // 啟動失敗 → fail_fast：終止已啟動的服務後回報錯誤
                terminate_all(&mut running);
                return Err(e);
            }
        };
        if let Some(fd) = spawned.stdout {
            spawn_forwarder(svc.name.clone(), fd, false);
        }
        if let Some(fd) = spawned.stderr {
            spawn_forwarder(svc.name.clone(), fd, true);
        }
        running.push(Running {
            mid: spawned.pid,
            init: spawned.init_pid,
            name: svc.name.clone(),
        });
        eprintln!(
            "[guest-agent] 服務 `{}` 已啟動（pid {}）",
            svc.name, spawned.pid
        );
    }

    monitor(&mut running)
}

/// 等待子行程結束；任一非零 → fail_fast 終止其餘並回傳該 code。
fn monitor(running: &mut Vec<Running>) -> Result<i32> {
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            eprintln!("[guest-agent] 收到終止訊號，正在停止所有服務…");
            terminate_all(running);
            return Ok(130);
        }
        if running.is_empty() {
            return Ok(0);
        }
        let status = match waitpid(None, None) {
            Ok(s) => s,
            Err(Errno::EINTR) => continue, // 可能是 SIGTERM/SIGINT，回頭檢查旗標
            Err(Errno::ECHILD) => return Ok(0),
            Err(e) => bail!("等待子行程失敗：{e}"),
        };
        let (pid, code) = match status {
            WaitStatus::Exited(pid, code) => (pid, code),
            WaitStatus::Signaled(pid, sig, _) => (pid, 128 + sig as i32),
            _ => continue,
        };
        // 我們 waitpid 到的是中繼行程（其 exit code 即服務本體的 exit code）。
        let Some(idx) = running.iter().position(|r| r.mid == pid) else {
            continue; // 非我們追蹤的子行程
        };
        let name = running.remove(idx).name;
        if code != 0 {
            eprintln!(
                "[guest-agent] 服務 `{name}` 以非零代碼 {code} 結束（fail_fast）；正在終止其餘服務…"
            );
            terminate_all(running);
            return Ok(code);
        }
        eprintln!("[guest-agent] 服務 `{name}` 正常結束");
    }
}

/// SIGTERM 全部 → 最多等 5 秒 → 仍存活者 SIGKILL（並回收殭屍行程）。
///
/// 同時對「中繼行程的 process group」與「服務本體（pid-ns init）的 host pid」送訊號：
/// 前者涵蓋一般情況；後者確保即使服務 setsid/setpgid 脫離中繼 group，
/// 仍能直接殺掉 pid-ns 的 init 而拆除整個 namespace（避免行程/埠洩漏）。
fn terminate_all(running: &mut Vec<Running>) {
    if running.is_empty() {
        return;
    }
    for r in running.iter() {
        let _ = killpg(r.mid, Signal::SIGTERM);
        if let Some(init) = r.init {
            let _ = kill(init, Signal::SIGTERM);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !running.is_empty() && Instant::now() < deadline {
        running.retain(|r| {
            !matches!(
                waitpid(r.mid, Some(WaitPidFlag::WNOHANG)),
                Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) | Err(_)
            )
        });
        if running.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for r in running.iter() {
        eprintln!(
            "[guest-agent] 服務 `{}` 未在 5 秒內結束，強制終止（SIGKILL）",
            r.name
        );
        let _ = killpg(r.mid, Signal::SIGKILL);
        if let Some(init) = r.init {
            let _ = kill(init, Signal::SIGKILL);
        }
        let _ = waitpid(r.mid, None);
    }
    running.clear();
}

/// 轉發子行程輸出：逐行加上 `[svc]` 前綴寫到自身 stdout / stderr。
fn spawn_forwarder(name: String, fd: OwnedFd, to_stderr: bool) {
    std::thread::spawn(move || {
        let file = std::fs::File::from(fd);
        let mut reader = BufReader::new(file);
        let prefix = format!("[{name}] ");
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break, // 管線關閉（服務結束）
                Ok(_) => {
                    let write_line = |w: &mut dyn Write| {
                        let _ = w.write_all(prefix.as_bytes());
                        let _ = w.write_all(&buf);
                        if !buf.ends_with(b"\n") {
                            let _ = w.write_all(b"\n");
                        }
                        let _ = w.flush();
                    };
                    if to_stderr {
                        write_line(&mut std::io::stderr().lock());
                    } else {
                        write_line(&mut std::io::stdout().lock());
                    }
                }
            }
        }
    });
}
