//! 以 Linux namespaces（mount + pid，rootless 時另加 user）啟動單一服務並 exec 有效命令。
//!
//! 流程（docs/DESIGN.md §6）：
//! supervisor fork 出「中繼行程」→ 中繼行程依執行身分 unshare namespaces
//! （**真實 root：MNT|PID，不開 user ns**；**非 root（rootless）：先 USER 寫映射——
//! 有 /etc/subuid 委派時由 supervisor 以 newuidmap 代寫範圍映射、否則自寫單一
//! uid/gid map——再 MNT|PID**）→ 再 fork 一次，孫行程成為新 pid namespace 的 pid 1 →
//! 孫行程完成掛載（proc、dev、persist、bind mounts、GUI socket）→
//! pivot_root → chdir → execve。中繼行程等待孫行程並轉送 exit code。
//!
//! 網路 **不** unshare（共享 host 網路 → ports 直接生效）。
//! 以真實 root 執行時不開 user namespace、服務以 root 跑——官方會 `chown`/`gosu` 到
//! 服務專用 uid（redis/postgres 999…）的映像因此可用；非 root 開 user ns，映射見
//! `subid.rs`（範圍委派 → chown/gosu 亦可用；無委派 → 單一 uid 映射，此類映像受限）。
//! image_config.user 在 v1 忽略。
//!
//! 本模組於 `lib.rs` 以 `#[cfg(target_os = "linux")]` 限定，故此處不再重複內層 cfg。

use std::convert::Infallible;
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use std::os::fd::BorrowedFd;

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork, setpgid};

/// 啟動一個服務所需的輸入。
pub struct SpawnSpec<'a> {
    pub service: &'a chefer_bundle::ServiceEntry,
    /// 最終 root 路徑：非 overlay = 合併好的 rootfs 目錄；overlay = 已建好的空 merged 掛載點。
    pub rootfs: &'a Path,
    /// app 資料目錄（persist 實體存放在 `<data_dir>/data/<svc>`）。
    pub data_dir: &'a Path,
    /// 是否為 terminal 介面服務（stdio 直通、不加前綴）。
    pub terminal: bool,
    /// internal/bridge 模式：要 setns 進入的 app netns（含 rootless 時的 user ns fd）。
    /// shared 模式為 None（不隔離網路，沿用共享 netns）。
    pub netns: Option<crate::netns::NetnsJoin>,
    /// Some = overlayfs 模式（root 後端）：在 `rootfs`（merged 掛載點）上掛 overlay。
    /// None = 直接 bind 合併好的 rootfs（rootless / 不支援 overlay）。
    pub overlay: Option<&'a OverlayDirs>,
    /// 此後端能否對容器做 GPU passthrough（僅原生 Linux / WSL2 為 true）。
    /// 服務 `gpu: true` 但此為 false（WHP/vz VM）→ 啟動時回明確錯誤。
    pub gpu_host: bool,
}

/// overlay 掛載所需的目錄：唯讀 lowerdir（base→top）+ 每次執行的可寫 upper/work。
#[derive(Clone, Debug)]
pub struct OverlayDirs {
    /// 各層 lowerdir，base→top 順序（掛載時會反轉成 overlay 要求的 top-first）。
    pub lowers: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// 一個服務的 rootfs 來源：最終 root 路徑 + （選用）overlay 目錄組。
pub struct ServiceRootfs {
    pub root: PathBuf,
    pub overlay: Option<OverlayDirs>,
}

/// 已啟動的服務（pid 為中繼行程；其 pgid 與 pid 相同，供整組送訊號）。
pub struct Spawned {
    pub pid: Pid,
    /// 服務本體（新 pid namespace 的 pid 1）在 host pid namespace 的 pid。
    /// 終止時直接對它送訊號——殺掉 pid-ns 的 init 會連帶拆除整個 namespace，
    /// 不受服務 setsid/setpgid 脫離中繼 process group 的影響。
    /// 中繼行程在二次 fork 後回報；取不到時為 None（退回只用 process group）。
    pub init_pid: Option<Pid>,
    /// 非 terminal 服務的 stdout 讀取端（供 supervisor 加前綴轉發）。
    pub stdout: Option<OwnedFd>,
    /// 非 terminal 服務的 stderr 讀取端。
    pub stderr: Option<OwnedFd>,
}

/// 一筆 bind mount 計畫（fork 前先算好，孫行程內只做系統呼叫）。
struct BindEntry {
    /// host 端來源路徑。
    host: PathBuf,
    /// rootfs 內的絕對目標路徑。
    target: PathBuf,
    read_only: bool,
    /// 來源是否為目錄（決定目標要建目錄還是空檔案）。
    is_dir: bool,
}

/// fork 前備妥的啟動計畫。
struct ChildPlan {
    name: String,
    rootfs: PathBuf,
    binds: Vec<BindEntry>,
    workdir: String,
    /// argv[0] 原字串（用於 PATH 搜尋）。
    program: String,
    argv: Vec<CString>,
    envp: Vec<CString>,
    /// 容器內 PATH（手動搜尋執行檔，避免 libc execvpe 行為差異）。
    path_env: String,
    uid: u32,
    gid: u32,
    terminal: bool,
    /// internal/bridge：要 setns 進入的 app netns；None = shared（共享 netns）。
    netns: Option<crate::netns::NetnsJoin>,
    /// Some = overlay 模式：在 rootfs（merged 掛載點）上掛 overlayfs。
    overlay: Option<OverlayDirs>,
}

/// 把容器內絕對路徑接到 rootfs 下（含安全檢查，拒絕 `..` 等）。
fn join_guest(rootfs: &Path, guest: &str) -> Result<PathBuf> {
    let stripped = guest.strip_prefix('/').unwrap_or(guest);
    let Some(rel) = crate::rootfs::sanitize_rel_path(stripped)? else {
        bail!("invalid in-container path (must not be the root directory or empty): {guest}");
    };
    Ok(rootfs.join(rel))
}

/// 啟動服務：fork 中繼行程，回傳其 pid 與（非 terminal 時的）輸出管線讀取端。
pub fn spawn_service(spec: &SpawnSpec) -> Result<Spawned> {
    let svc = spec.service;
    let plan = build_plan(spec)?;

    // 非 terminal：建立 stdout/stderr 管線（O_CLOEXEC；子行程 dup2 後原 fd 隨 exec 關閉）
    let (out_r, out_w, err_r, err_w) = if spec.terminal {
        (None, None, None, None)
    } else {
        let (or_, ow) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .context("failed to create stdout pipe")?;
        let (er, ew) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .context("failed to create stderr pipe")?;
        (Some(or_), Some(ow), Some(er), Some(ew))
    };

    // pid 回報管線：中繼行程在二次 fork 後，把孫行程（pid-ns init）的 host pid
    // 寫回給 supervisor。CLOEXEC：服務 exec 後不繼承。
    let (pid_r, pid_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .context("failed to create pid report pipe")?;

    // rootless + shared 模式且有 subuid 委派：建立映射握手管線，由 supervisor 對
    // 中繼行程代寫範圍 uid/gid map（newuidmap）——chown/gosu image 因此可用。
    // internal/bridge 的服務走 holder 的 user ns（netns.rs 已套委派），此處不需要。
    let delegation = if plan.netns.is_none() && !nix::unistd::geteuid().is_root() {
        crate::subid::delegation()
    } else {
        None
    };
    let map_sync = delegation
        .map(|_| crate::subid::MapSync::new())
        .transpose()
        .context("failed to create id-map sync pipes")?;

    // SAFETY: fork 後子行程僅呼叫 async-signal 相對安全的操作與系統呼叫，
    // 失敗一律以 _exit 結束，不返回呼叫端。
    match unsafe { fork() }.with_context(|| format!("failed to fork service `{}`", svc.name))? {
        ForkResult::Parent { child } => {
            // 與子行程雙邊 setpgid，消除 killpg 競態
            let _ = setpgid(child, child);
            drop(out_w);
            drop(err_w);
            drop(pid_w);
            if let (Some(d), Some(sync)) = (delegation, &map_sync) {
                crate::subid::supervisor_write_maps(d, child, &sync.unshared_r, &sync.verdict_w);
            }
            drop(map_sync);
            // 讀取孫行程 host pid（4 bytes LE i32）；中繼行程二次 fork 失敗時
            // 管線會被關閉而讀到 EOF → init_pid = None。
            let init_pid = read_init_pid(pid_r);
            Ok(Spawned {
                pid: child,
                init_pid,
                stdout: out_r,
                stderr: err_r,
            })
        }
        ForkResult::Child => {
            drop(out_r);
            drop(err_r);
            drop(pid_r);
            let child_sync = map_sync.map(|s| {
                drop(s.unshared_r);
                drop(s.verdict_w);
                (s.unshared_w, s.verdict_r)
            });
            let code = middle_child(&plan, out_w, err_w, pid_w, child_sync);
            unsafe { libc::_exit(code) }
        }
    }
}

/// 從 pid 回報管線讀回孫行程 host pid（4 bytes LE i32）；失敗/EOF → None。
fn read_init_pid(pid_r: OwnedFd) -> Option<Pid> {
    use std::io::Read;
    let mut f = std::fs::File::from(pid_r);
    let mut buf = [0u8; 4];
    match f.read_exact(&mut buf) {
        Ok(()) => {
            let raw = i32::from_le_bytes(buf);
            if raw > 0 {
                Some(Pid::from_raw(raw))
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// fork 前備妥所有計算（路徑、argv/env CString、bind 清單）。
fn build_plan(spec: &SpawnSpec) -> Result<ChildPlan> {
    let svc = spec.service;
    let argv = svc.effective_command()?;
    let mut env = svc.effective_env();
    let mut binds: Vec<BindEntry> = Vec::new();

    // persist：host `<data_dir>/data/<svc>` ↔ 容器內 persist_path
    if let Some(persist) = &svc.persist_path {
        let host = spec.data_dir.join("data").join(&svc.name);
        fs::create_dir_all(&host)
            .with_context(|| format!("failed to create persist directory: {}", host.display()))?;
        binds.push(BindEntry {
            host,
            target: join_guest(spec.rootfs, persist)?,
            read_only: false,
            is_dir: true,
        });
    }

    // 使用者宣告的 bind mounts（host 路徑存在性已於啟動前整體驗證）
    for m in &svc.mounts {
        let host = PathBuf::from(&m.host);
        let is_dir = host.is_dir();
        binds.push(BindEntry {
            host,
            target: join_guest(spec.rootfs, &m.guest)?,
            read_only: m.read_only,
            is_dir,
        });
    }

    // GPU passthrough（opt-in per-service `gpu: true`；見 docs/DESIGN.md「GPU passthrough」）。
    // 只在能實際觸及 host GPU 的後端可行（原生 Linux / WSL2）；WHP/vz VM 明確報錯，
    // 不靜默綁到 virtio-gpu 的 2D `/dev/dri`。探測到的節點/libs 以既有 bind 流程套用
    //（在 setup_dev 建好 /dev tmpfs 之後）。
    if svc.gpu {
        if !spec.gpu_host {
            bail!(
                "service `{}` requests `gpu: true`, but GPU passthrough is not available on this backend. \
It works only on native Linux and the Windows WSL2 backend; the Windows WHP micro-VM and the macOS VM cannot expose a host GPU to a Linux container. \
On Windows, use the WSL2 backend for GPU (a bare WHP micro-VM has no GPU paravirtualization).",
                svc.name
            );
        }
        let gpu = crate::gpu::collect();
        if gpu.is_empty() {
            bail!(
                "service `{}` requests `gpu: true`, but no GPU device nodes were found on the host \
(searched /dev/dxg, /dev/dri, /dev/nvidia*). \
Ensure a GPU with drivers is present: on native Linux install the vendor driver (exposes /dev/nvidia* and/or /dev/dri); \
on WSL2 use a recent GPU driver with WSL support so /dev/dxg appears.",
                svc.name
            );
        }
        for b in &gpu.binds {
            binds.push(BindEntry {
                host: b.host.clone(),
                target: join_guest(spec.rootfs, &b.guest)?,
                read_only: b.read_only,
                is_dir: b.is_dir,
            });
        }
        // WSL2：把 host 驅動 lib 目錄（含版本相符的 libcuda 等）接進容器 LD_LIBRARY_PATH
        //（附加於既有值後，不覆蓋 image 自身設定）。
        if !gpu.ld_library_paths.is_empty() {
            let extra = gpu.ld_library_paths.join(":");
            env.entry("LD_LIBRARY_PATH".to_string())
                .and_modify(|v| {
                    *v = if v.is_empty() {
                        extra.clone()
                    } else {
                        format!("{v}:{extra}")
                    };
                })
                .or_insert(extra);
        }
    }

    // GUI：存在才掛 X11 / Wayland socket，並補上對應環境變數（不覆蓋使用者設定）
    if svc.interface_mode.wants_gui() {
        let x11 = Path::new("/tmp/.X11-unix");
        if x11.is_dir() {
            binds.push(BindEntry {
                host: x11.to_path_buf(),
                target: join_guest(spec.rootfs, "/tmp/.X11-unix")?,
                read_only: false,
                is_dir: true,
            });
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
            env.entry("DISPLAY".to_string()).or_insert(display);
        }
        let mut xdg_candidates = Vec::new();
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            xdg_candidates.push(xdg);
        }
        let wslg_runtime = "/mnt/wslg/runtime-dir";
        if Path::new(wslg_runtime).is_dir() && !xdg_candidates.iter().any(|xdg| xdg == wslg_runtime)
        {
            xdg_candidates.push(wslg_runtime.to_string());
        }
        for xdg in xdg_candidates {
            let xdg_dir = Path::new(&xdg);
            let mut wayland_names: Vec<String> = Vec::new();
            if let Ok(rd) = fs::read_dir(xdg_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    // 只掛 socket 本體（wayland-0 等），略過 lock 檔
                    if name.starts_with("wayland-") && !name.ends_with(".lock") {
                        let Ok(file_type) = e.file_type() else {
                            continue;
                        };
                        if !file_type.is_socket() {
                            continue;
                        }
                        binds.push(BindEntry {
                            host: e.path(),
                            target: join_guest(spec.rootfs, &format!("{xdg}/{name}"))?,
                            read_only: false,
                            is_dir: false,
                        });
                        wayland_names.push(name);
                    }
                }
            }
            if !wayland_names.is_empty() {
                wayland_names.sort();
                env.entry("XDG_RUNTIME_DIR".to_string())
                    .or_insert(xdg.clone());
                let wd = std::env::var("WAYLAND_DISPLAY")
                    .ok()
                    .filter(|name| wayland_names.iter().any(|candidate| candidate == name))
                    .unwrap_or_else(|| wayland_names[0].clone());
                env.entry("WAYLAND_DISPLAY".to_string()).or_insert(wd);
                break;
            }
        }
    }

    let workdir = svc
        .workdir_override
        .clone()
        .or_else(|| {
            svc.image_config
                .working_dir
                .clone()
                .filter(|w| !w.is_empty())
        })
        .unwrap_or_else(|| "/".to_string());

    let path_env = env.get("PATH").cloned().unwrap_or_default();
    let argv_c = argv
        .iter()
        .map(|s| CString::new(s.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .context("command argument contains a NUL byte, cannot execute")?;
    let envp = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<Vec<_>, _>>()
        .context("environment variable contains a NUL byte, cannot execute")?;

    Ok(ChildPlan {
        name: svc.name.clone(),
        rootfs: spec.rootfs.to_path_buf(),
        binds,
        workdir,
        program: argv[0].clone(),
        argv: argv_c,
        envp,
        path_env,
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        terminal: spec.terminal,
        netns: spec.netns,
        overlay: spec.overlay.cloned(),
    })
}

/// 中繼行程：設定 stdio / pgid / namespaces，再 fork 出孫行程並轉送其 exit code。
/// `pid_w`：把孫行程（pid-ns init）的 host pid 回報給 supervisor 的管線寫入端。
fn middle_child(
    plan: &ChildPlan,
    stdout_w: Option<OwnedFd>,
    stderr_w: Option<OwnedFd>,
    pid_w: OwnedFd,
    // Some = supervisor 有 subuid 委派：unshare(NEWUSER) 後握手等它代寫範圍映射
    //（(unshared_w, verdict_r)，見 subid.rs）。None = 自寫單一映射（現行 fallback）。
    map_sync: Option<(OwnedFd, OwnedFd)>,
) -> i32 {
    // stdio 重導（terminal 服務維持繼承）
    unsafe {
        if let Some(fd) = &stdout_w {
            libc::dup2(fd.as_raw_fd(), 1);
        }
        if let Some(fd) = &stderr_w {
            libc::dup2(fd.as_raw_fd(), 2);
        }
        if !plan.terminal {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if null >= 0 {
                libc::dup2(null, 0);
                if null > 2 {
                    libc::close(null);
                }
            }
        }
    }

    // 自成 process group：supervisor 以 killpg 對整組（中繼 + 服務本體）送訊號
    let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
    // 中繼行程忽略 TERM/INT：訊號交給服務本體處理，自己留著轉送 exit code
    unsafe {
        let _ = signal(Signal::SIGTERM, SigHandler::SigIgn);
        let _ = signal(Signal::SIGINT, SigHandler::SigIgn);
    }

    // 以真實 root 執行時（WSL2 distro、macOS VM 內），**不**開 user namespace：
    // root 對完整 uid 範圍本就有 CAP_SETUID/CAP_CHOWN，容器 entrypoint 的
    // chown/gosu/setuid 到服務專用 uid（官方 redis/postgres 的 999…）可直接成功。
    //
    // 非 root（原生 Linux rootless）才需要 NEWUSER 取得 ns 內 root 以做 pivot_root/mount。
    // 映射有兩條路（subid.rs）：核心規定 unshare(NEWUSER) 後自寫 uid_map 只能映射單一
    // uid，範圍映射需由仍在 parent ns 的特權行程代寫——有 /etc/subuid 委派時由 supervisor
    // 以 newuidmap/newgidmap 代寫範圍映射（chown/gosu image 可用，同 rootless podman）；
    // 無委派則自寫單一映射（此類映像受限）。
    let real_root = nix::unistd::geteuid().is_root();
    if let Some(join) = plan.netns {
        // internal/bridge：加入既有的 app netns（rootless 另先加入 holder 的 user ns
        // 以取得 caps，並沿用 holder 已寫好的 uid/gid 映射 → 自己不再 unshare/寫 map）。
        if let Err(code) = join_app_netns(&plan.name, join, real_root) {
            return code;
        }
    } else {
        // shared：沿用共享 netns。以真實 root 執行時不開 user ns（chown/gosu 直接可行）；
        // 非 root 先單獨 unshare(NEWUSER) 寫映射（有 subuid 委派時由 supervisor 以
        // newuidmap 代寫範圍映射 → chown/gosu 可用；否則自寫單一 uid/gid map），
        // 再 unshare(MNT|PID)。
        if !real_root {
            if let Err(e) = unshare(CloneFlags::CLONE_NEWUSER) {
                eprintln!(
                    "[guest-agent] service `{}` unshare(user) failed: {e}; \
                     make sure the kernel allows unprivileged user namespaces (/proc/sys/kernel/unprivileged_userns_clone = 1)",
                    plan.name
                );
                return 126;
            }
            let delegated = map_sync
                .as_ref()
                .is_some_and(|(w, r)| crate::subid::child_wait_maps(w, r));
            if !delegated && let Err(e) = write_id_maps(plan.uid, plan.gid) {
                eprintln!(
                    "[guest-agent] service `{}` failed to write uid/gid map: {e:#}",
                    plan.name
                );
                return 126;
            }
        }
        drop(map_sync);
        if let Err(e) = unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID) {
            eprintln!(
                "[guest-agent] service `{}` unshare(mount+pid) failed: {e}",
                plan.name
            );
            return 126;
        }
    }

    // unshare(PID) 只影響之後 fork 的子行程 → 再 fork 一次，孫行程成為新 ns 的 pid 1
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            // 回報孫行程 host pid 給 supervisor（供 setsid/setpgid 後仍能直接終止）
            let raw = child.as_raw().to_le_bytes();
            let _ = nix::unistd::write(&pid_w, &raw);
            drop(pid_w);
            loop {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => return code,
                    Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
                    Ok(_) => continue,
                    Err(Errno::EINTR) => continue,
                    Err(e) => {
                        eprintln!(
                            "[guest-agent] failed to wait for service `{}`: {e}",
                            plan.name
                        );
                        return 126;
                    }
                }
            }
        }
        Ok(ForkResult::Child) => {
            drop(pid_w);
            let err = match setup_and_exec(plan) {
                Err(e) => e,
                Ok(never) => match never {},
            };
            eprintln!(
                "[guest-agent] service `{}` failed to start: {err:#}",
                plan.name
            );
            unsafe { libc::_exit(126) }
        }
        Err(e) => {
            eprintln!(
                "[guest-agent] service `{}` second fork failed: {e}",
                plan.name
            );
            126
        }
    }
}

/// internal/bridge 模式：把當前（中繼）行程加入既有的 app netns，再 unshare(MNT|PID)。
///
/// - **rootless**（`join.user` 為 Some）：先 `setns` 進 holder 的 user ns（取得 ns 內
///   root 與 CAP_SYS_ADMIN，並沿用 holder 的 uid/gid 映射——有 subuid 委派時為範圍映射，
///   chown/gosu 因此可用），再 `setns` 進 netns，
///   最後 `unshare(NEWNS|NEWPID)`。**不**再開新 user ns、**不**重寫 uid map。
/// - **root**（`join.user` 為 None）：直接 `setns` 進 netns，再 `unshare(NEWNS|NEWPID)`。
///
/// 失敗回 `Err(126)`（與其他啟動前置失敗一致）。
fn join_app_netns(name: &str, join: crate::netns::NetnsJoin, _real_root: bool) -> Result<(), i32> {
    if let Some(user_fd) = join.user {
        // SAFETY: user_fd 由 supervisor 經 fork 繼承，於本行程使用期間有效。
        let fd = unsafe { BorrowedFd::borrow_raw(user_fd) };
        if let Err(e) = setns(fd, CloneFlags::CLONE_NEWUSER) {
            eprintln!("[guest-agent] service `{name}` failed to join the app user ns: {e}");
            return Err(126);
        }
    }
    // SAFETY: net_fd 由 supervisor 經 fork 繼承，於本行程使用期間有效。
    let net = unsafe { BorrowedFd::borrow_raw(join.net) };
    if let Err(e) = setns(net, CloneFlags::CLONE_NEWNET) {
        eprintln!("[guest-agent] service `{name}` failed to join the app network ns: {e}");
        return Err(126);
    }
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID) {
        eprintln!(
            "[guest-agent] service `{name}` unshare(mount+pid) failed: {e}; \
             make sure the kernel allows the required namespaces"
        );
        return Err(126);
    }
    Ok(())
}

/// 在新 user namespace 內設定**單一** uid/gid 映射——rootless 且**沒有** subuid 委派
/// 時的 fallback（有委派時由 supervisor 以 newuidmap 代寫範圍映射，見 `subid.rs`；
/// root 不開 NEWUSER，見 `middle_child` 內的 `real_root` 分支）。
///
/// 把容器內 uid/gid 0 映射到呼叫者自身的 uid/gid，並先 **deny** setgroups。
/// 核心規定：unshare(NEWUSER) 後行程自寫自身 uid_map 只能映射單一 id，故此 fallback
/// 下會 chown 到其他服務 uid 的映像受限（等同 rootless podman 無 /etc/subuid 委派）。
fn write_id_maps(uid: u32, gid: u32) -> Result<()> {
    // 寫單行 gid_map 前必須 deny setgroups。
    match fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        // 舊核心（< 3.19）沒有此檔案 → 可直接寫 gid_map
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("failed to write /proc/self/setgroups"),
    }
    fs::write("/proc/self/uid_map", format!("0 {uid} 1"))
        .context("failed to write /proc/self/uid_map")?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1"))
        .context("failed to write /proc/self/gid_map")?;
    Ok(())
}

/// mount 輔助：固定泛型參數型別，呼叫端不必每次標註 `None::<&str>`。
fn mnt(
    src: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> nix::Result<()> {
    mount(src, target, fstype, flags, data)
}

/// 孫行程（新 pid ns 的 pid 1）：掛載、pivot_root、chdir、execve。
/// 成功時不返回；任何錯誤返回給呼叫端印出後 _exit。
fn setup_and_exec(plan: &ChildPlan) -> Result<Infallible> {
    let root = plan.rootfs.as_path();

    // 1) 掛載傳播設為 private（遞迴），避免影響 host
    mnt(
        None,
        Path::new("/"),
        None,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None,
    )
    .context("failed to set mount propagation to private")?;

    // 2) 準備 root 掛載點：overlay 模式掛 overlayfs（lower=各層、upper/work=本次執行可寫層）；
    //    否則把合併好的 rootfs bind 到自身成為掛載點（pivot_root 的前提）。
    if let Some(ov) = &plan.overlay {
        // overlay 的 lowerdir 需「上層在前」：lowers 為 base→top，故反轉。
        let lowerdir = ov
            .lowers
            .iter()
            .rev()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let opts = format!(
            "lowerdir={lowerdir},upperdir={},workdir={}",
            ov.upper.display(),
            ov.work.display()
        );
        mnt(
            Some(Path::new("overlay")),
            root,
            Some("overlay"),
            MsFlags::empty(),
            Some(&opts),
        )
        .with_context(|| format!("failed to mount overlay rootfs at {}", root.display()))?;
    } else {
        mnt(
            Some(root),
            root,
            None,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None,
        )
        .with_context(|| format!("failed to bind rootfs: {}", root.display()))?;
    }

    // 3) /proc（proc 型別；此時已在新 pid ns）
    let proc_dir = root.join("proc");
    fs::create_dir_all(&proc_dir).context("failed to create /proc directory")?;
    mnt(
        Some(Path::new("proc")),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None,
    )
    .context("failed to mount /proc")?;

    // 4) /dev
    setup_dev(root)?;

    // 5) persist / bind mounts / GUI socket
    for b in &plan.binds {
        if b.is_dir {
            fs::create_dir_all(&b.target).with_context(|| {
                format!(
                    "failed to create mount target directory: {}",
                    b.target.display()
                )
            })?;
        } else {
            if let Some(parent) = b.target.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create mount target parent directory: {}",
                        parent.display()
                    )
                })?;
            }
            if !b.target.exists() {
                fs::File::create(&b.target).with_context(|| {
                    format!("failed to create mount target file: {}", b.target.display())
                })?;
            }
        }
        mnt(
            Some(b.host.as_path()),
            &b.target,
            None,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None,
        )
        .with_context(|| {
            format!(
                "bind mount failed: {} → {}",
                b.host.display(),
                b.target.display()
            )
        })?;
        if b.read_only {
            mnt(
                None,
                &b.target,
                None,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None,
            )
            .with_context(|| format!("failed to set read-only mount: {}", b.target.display()))?;
        }
    }

    // 6) 切換根目錄：優先 pivot_root（隔離最完整）；若 / 是 initramfs rootfs，
    //    kernel 會以 EINVAL 拒絕 pivot_root，此時退回 MS_MOVE + chroot
    //    （macOS vz / QEMU appliance 從 initramfs 開機即屬此情形）。
    let put_old = root.join(".chefer-put-old");
    fs::create_dir_all(&put_old).context("failed to create pivot_root temp directory")?;
    match nix::unistd::pivot_root(root, &put_old) {
        Ok(()) => {
            nix::unistd::chdir("/").context("chdir(/) failed")?;
            umount2("/.chefer-put-old", MntFlags::MNT_DETACH)
                .context("failed to unmount old root")?;
            let _ = fs::remove_dir("/.chefer-put-old");
        }
        Err(Errno::EINVAL) => {
            // initramfs rootfs 無法 pivot_root。改用 switch_root 慣用法：
            // chdir(新根) → MS_MOVE 把新根掛載移成 / → chroot(".")。
            let _ = fs::remove_dir(&put_old); // 此路徑不需 put_old
            nix::unistd::chdir(root).context("chdir(new root) failed")?;
            mnt(
                Some(Path::new(".")),
                Path::new("/"),
                None,
                MsFlags::MS_MOVE,
                None,
            )
            .context("MS_MOVE of new root to / failed (pivot_root fallback path)")?;
            nix::unistd::chroot(".").context("chroot(new root) failed")?;
            nix::unistd::chdir("/").context("chdir(/) failed")?;
        }
        Err(e) => return Err(anyhow::anyhow!("pivot_root failed: {e}")),
    }

    // 7) chdir 至工作目錄（workdir_override > image working_dir > /）
    nix::unistd::chdir(Path::new(&plan.workdir)).with_context(|| {
        format!(
            "failed to change to working directory `{}`; make sure the image contains this directory or fix the workdir setting",
            plan.workdir
        )
    })?;

    // 8) execve 前把 SIGTERM/SIGINT 還原為 SIG_DFL。
    //    中繼行程曾把這兩個訊號設為 SIG_IGN；POSIX 規定 execve 會把「被捕捉」的
    //    訊號還原為預設，但「被忽略(SIG_IGN)」的維持忽略。若不還原，服務本體
    //    與其子行程啟動時即忽略 SIGTERM/SIGINT，破壞 supervisor 的優雅終止
    //    （SIGTERM → 5s → SIGKILL）契約。容器 runtime 的標準做法。
    unsafe {
        let _ = signal(Signal::SIGTERM, SigHandler::SigDfl);
        let _ = signal(Signal::SIGINT, SigHandler::SigDfl);
    }

    // 9) 解析執行檔（手動 PATH 搜尋，避免各 libc 的 execvpe 行為差異）並 execve
    let prog = resolve_program(&plan.program, &plan.path_env)?;
    nix::unistd::execve(&prog, &plan.argv, &plan.envp).with_context(|| {
        format!(
            "failed to execute `{}`; make sure the file exists and is executable (or that its interpreter is present in the image)",
            plan.program
        )
    })?;
    unreachable!("execve does not return on success");
}

/// 建立容器的 /dev：tmpfs + bind host 基本裝置 + devpts + shm + 慣用 symlink。
fn setup_dev(root: &Path) -> Result<()> {
    let dev = root.join("dev");
    fs::create_dir_all(&dev).context("failed to create /dev directory")?;
    mnt(
        Some(Path::new("tmpfs")),
        &dev,
        Some("tmpfs"),
        MsFlags::MS_NOSUID,
        Some("mode=755,size=65536k"),
    )
    .context("failed to mount /dev (tmpfs)")?;

    // host 基本裝置 bind 進來（不存在者略過——如無 controlling tty 時的 /dev/tty）
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let host = Path::new("/dev").join(name);
        if !host.exists() {
            continue;
        }
        let target = dev.join(name);
        fs::File::create(&target)
            .with_context(|| format!("failed to create /dev/{name} mount point"))?;
        mnt(Some(host.as_path()), &target, None, MsFlags::MS_BIND, None)
            .with_context(|| format!("failed to bind /dev/{name}"))?;
    }

    // /dev/pts（devpts 新實例；gid 未映射 → 不指定 gid 選項）
    let pts = dev.join("pts");
    fs::create_dir_all(&pts).context("failed to create /dev/pts")?;
    mnt(
        Some(Path::new("devpts")),
        &pts,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )
    .context("failed to mount /dev/pts")?;

    // /dev/shm（tmpfs，1777）
    let shm = dev.join("shm");
    fs::create_dir_all(&shm).context("failed to create /dev/shm")?;
    mnt(
        Some(Path::new("tmpfs")),
        &shm,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777,size=65536k"),
    )
    .context("failed to mount /dev/shm")?;

    // 慣用 symlink
    let links = [
        ("ptmx", "pts/ptmx"),
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ];
    for (name, target) in links {
        let _ = std::os::unix::fs::symlink(target, dev.join(name));
    }
    Ok(())
}

/// 在容器內 PATH 中尋找可執行檔（argv[0] 含 `/` 時直接使用）。
fn resolve_program(program: &str, path_env: &str) -> Result<CString> {
    if program.contains('/') {
        return CString::new(program).context("executable path contains a NUL byte");
    }
    for dir in path_env.split(':').filter(|d| !d.is_empty()) {
        let cand = Path::new(dir).join(program);
        if let Ok(md) = cand.metadata()
            && md.is_file()
            && (md.permissions().mode() & 0o111) != 0
        {
            return CString::new(cand.as_os_str().as_bytes())
                .context("executable path contains a NUL byte");
        }
    }
    bail!(
        "executable not found: `{program}` (searched PATH={path_env}); \
         make sure the image contains this command, or specify a full path in the appcipe `cmd`"
    )
}
