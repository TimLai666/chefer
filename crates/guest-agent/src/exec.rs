//! 以 Linux namespaces（user + mount + pid）啟動單一服務並 exec 有效命令。
//!
//! 流程（docs/DESIGN.md §6）：
//! supervisor fork 出「中繼行程」→ 中繼行程 unshare(USER|MNT|PID) 並寫入
//! uid/gid map → 再 fork 一次，孫行程成為新 pid namespace 的 pid 1 →
//! 孫行程完成掛載（proc、dev、persist、bind mounts、GUI socket）→
//! pivot_root → chdir → execve。中繼行程等待孫行程並轉送 exit code。
//!
//! 網路 **不** unshare（共享 host 網路 → ports 直接生效）。
//! image_config.user 在 v1 忽略：單一 uid map（目前 uid ↔ 0）下無對應意義。
#![cfg(target_os = "linux")]

use std::convert::Infallible;
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork, setpgid};

/// 啟動一個服務所需的輸入。
pub struct SpawnSpec<'a> {
    pub service: &'a chefer_bundle::ServiceEntry,
    /// 已組裝完成的 rootfs 目錄。
    pub rootfs: &'a Path,
    /// app 資料目錄（persist 實體存放在 `<data_dir>/data/<svc>`）。
    pub data_dir: &'a Path,
    /// 是否為 terminal 介面服務（stdio 直通、不加前綴）。
    pub terminal: bool,
}

/// 已啟動的服務（pid 為中繼行程；其 pgid 與 pid 相同，供整組送訊號）。
pub struct Spawned {
    pub pid: Pid,
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
}

/// 把容器內絕對路徑接到 rootfs 下（含安全檢查，拒絕 `..` 等）。
fn join_guest(rootfs: &Path, guest: &str) -> Result<PathBuf> {
    let stripped = guest.strip_prefix('/').unwrap_or(guest);
    let Some(rel) = crate::rootfs::sanitize_rel_path(stripped)? else {
        bail!("容器內路徑無效（不可為根目錄或空路徑）：{guest}");
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
        let (or_, ow) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).context("建立 stdout 管線失敗")?;
        let (er, ew) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).context("建立 stderr 管線失敗")?;
        (Some(or_), Some(ow), Some(er), Some(ew))
    };

    // SAFETY: fork 後子行程僅呼叫 async-signal 相對安全的操作與系統呼叫，
    // 失敗一律以 _exit 結束，不返回呼叫端。
    match unsafe { fork() }.with_context(|| format!("fork 服務 `{}` 失敗", svc.name))? {
        ForkResult::Parent { child } => {
            // 與子行程雙邊 setpgid，消除 killpg 競態
            let _ = setpgid(child, child);
            drop(out_w);
            drop(err_w);
            Ok(Spawned {
                pid: child,
                stdout: out_r,
                stderr: err_r,
            })
        }
        ForkResult::Child => {
            drop(out_r);
            drop(err_r);
            let code = middle_child(&plan, out_w, err_w);
            unsafe { libc::_exit(code) }
        }
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
            .with_context(|| format!("建立 persist 目錄失敗：{}", host.display()))?;
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
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            let xdg_dir = Path::new(&xdg);
            let mut wayland_names: Vec<String> = Vec::new();
            if let Ok(rd) = fs::read_dir(xdg_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    // 只掛 socket 本體（wayland-0 等），略過 lock 檔
                    if name.starts_with("wayland-") && !name.ends_with(".lock") {
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
                let wd =
                    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| wayland_names[0].clone());
                env.entry("WAYLAND_DISPLAY".to_string()).or_insert(wd);
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
        .context("命令參數含 NUL 字元，無法執行")?;
    let envp = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<Vec<_>, _>>()
        .context("環境變數含 NUL 字元，無法執行")?;

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
    })
}

/// 中繼行程：設定 stdio / pgid / namespaces，再 fork 出孫行程並轉送其 exit code。
fn middle_child(plan: &ChildPlan, stdout_w: Option<OwnedFd>, stderr_w: Option<OwnedFd>) -> i32 {
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

    if let Err(e) =
        unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID)
    {
        eprintln!(
            "[guest-agent] 服務 `{}` unshare(user+mount+pid) 失敗：{e}；\
             請確認核心允許 unprivileged user namespaces（/proc/sys/kernel/unprivileged_userns_clone = 1）",
            plan.name
        );
        return 126;
    }
    if let Err(e) = write_id_maps(plan.uid, plan.gid) {
        eprintln!(
            "[guest-agent] 服務 `{}` 寫入 uid/gid map 失敗：{e:#}",
            plan.name
        );
        return 126;
    }

    // unshare(PID) 只影響之後 fork 的子行程 → 再 fork 一次，孫行程成為新 ns 的 pid 1
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => return code,
                Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
                Ok(_) => continue,
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    eprintln!("[guest-agent] 等待服務 `{}` 失敗：{e}", plan.name);
                    return 126;
                }
            }
        },
        Ok(ForkResult::Child) => {
            let err = match setup_and_exec(plan) {
                Err(e) => e,
                Ok(never) => match never {},
            };
            eprintln!("[guest-agent] 服務 `{}` 啟動失敗：{err:#}", plan.name);
            unsafe { libc::_exit(126) }
        }
        Err(e) => {
            eprintln!("[guest-agent] 服務 `{}` 二次 fork 失敗：{e}", plan.name);
            126
        }
    }
}

/// 在新 user namespace 內把目前 uid/gid 映射為 0（須先 deny setgroups）。
fn write_id_maps(uid: u32, gid: u32) -> Result<()> {
    match fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        // 舊核心（< 3.19）沒有此檔案 → 可直接寫 gid_map
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("寫入 /proc/self/setgroups 失敗"),
    }
    fs::write("/proc/self/uid_map", format!("0 {uid} 1"))
        .context("寫入 /proc/self/uid_map 失敗")?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1"))
        .context("寫入 /proc/self/gid_map 失敗")?;
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
    .context("設定掛載傳播為 private 失敗")?;

    // 2) rootfs bind 到自身，成為掛載點（pivot_root 的前提）
    mnt(
        Some(root),
        root,
        None,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None,
    )
    .with_context(|| format!("bind rootfs 失敗：{}", root.display()))?;

    // 3) /proc（proc 型別；此時已在新 pid ns）
    let proc_dir = root.join("proc");
    fs::create_dir_all(&proc_dir).context("建立 /proc 目錄失敗")?;
    mnt(
        Some(Path::new("proc")),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None,
    )
    .context("掛載 /proc 失敗")?;

    // 4) /dev
    setup_dev(root)?;

    // 5) persist / bind mounts / GUI socket
    for b in &plan.binds {
        if b.is_dir {
            fs::create_dir_all(&b.target)
                .with_context(|| format!("建立掛載目標目錄失敗：{}", b.target.display()))?;
        } else {
            if let Some(parent) = b.target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("建立掛載目標上層目錄失敗：{}", parent.display()))?;
            }
            if !b.target.exists() {
                fs::File::create(&b.target)
                    .with_context(|| format!("建立掛載目標檔案失敗：{}", b.target.display()))?;
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
                "bind mount 失敗：{} → {}",
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
            .with_context(|| format!("設定唯讀掛載失敗：{}", b.target.display()))?;
        }
    }

    // 6) pivot_root（put_old 用 rootfs 內暫目錄，detach 後刪除）
    let put_old = root.join(".chefer-put-old");
    fs::create_dir_all(&put_old).context("建立 pivot_root 暫目錄失敗")?;
    nix::unistd::pivot_root(root, &put_old).context("pivot_root 失敗")?;
    nix::unistd::chdir("/").context("chdir(/) 失敗")?;
    umount2("/.chefer-put-old", MntFlags::MNT_DETACH).context("卸載舊根失敗")?;
    let _ = fs::remove_dir("/.chefer-put-old");

    // 7) chdir 至工作目錄（workdir_override > image working_dir > /）
    nix::unistd::chdir(Path::new(&plan.workdir)).with_context(|| {
        format!(
            "切換至工作目錄 `{}` 失敗；請確認 image 內含該目錄或修正 workdir 設定",
            plan.workdir
        )
    })?;

    // 8) 解析執行檔（手動 PATH 搜尋，避免各 libc 的 execvpe 行為差異）並 execve
    let prog = resolve_program(&plan.program, &plan.path_env)?;
    nix::unistd::execve(&prog, &plan.argv, &plan.envp).with_context(|| {
        format!(
            "執行 `{}` 失敗；請確認該檔案存在且可執行（或其直譯器存在於 image 內）",
            plan.program
        )
    })?;
    unreachable!("execve 成功時不會返回");
}

/// 建立容器的 /dev：tmpfs + bind host 基本裝置 + devpts + shm + 慣用 symlink。
fn setup_dev(root: &Path) -> Result<()> {
    let dev = root.join("dev");
    fs::create_dir_all(&dev).context("建立 /dev 目錄失敗")?;
    mnt(
        Some(Path::new("tmpfs")),
        &dev,
        Some("tmpfs"),
        MsFlags::MS_NOSUID,
        Some("mode=755,size=65536k"),
    )
    .context("掛載 /dev (tmpfs) 失敗")?;

    // host 基本裝置 bind 進來（不存在者略過——如無 controlling tty 時的 /dev/tty）
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let host = Path::new("/dev").join(name);
        if !host.exists() {
            continue;
        }
        let target = dev.join(name);
        fs::File::create(&target).with_context(|| format!("建立 /dev/{name} 掛載點失敗"))?;
        mnt(Some(host.as_path()), &target, None, MsFlags::MS_BIND, None)
            .with_context(|| format!("bind /dev/{name} 失敗"))?;
    }

    // /dev/pts（devpts 新實例；gid 未映射 → 不指定 gid 選項）
    let pts = dev.join("pts");
    fs::create_dir_all(&pts).context("建立 /dev/pts 失敗")?;
    mnt(
        Some(Path::new("devpts")),
        &pts,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )
    .context("掛載 /dev/pts 失敗")?;

    // /dev/shm（tmpfs，1777）
    let shm = dev.join("shm");
    fs::create_dir_all(&shm).context("建立 /dev/shm 失敗")?;
    mnt(
        Some(Path::new("tmpfs")),
        &shm,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777,size=65536k"),
    )
    .context("掛載 /dev/shm 失敗")?;

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
        return CString::new(program).context("執行檔路徑含 NUL 字元");
    }
    for dir in path_env.split(':').filter(|d| !d.is_empty()) {
        let cand = Path::new(dir).join(program);
        if let Ok(md) = cand.metadata() {
            if md.is_file() && (md.permissions().mode() & 0o111) != 0 {
                return CString::new(cand.as_os_str().as_bytes()).context("執行檔路徑含 NUL 字元");
            }
        }
    }
    bail!(
        "找不到可執行檔 `{program}`（搜尋 PATH={path_env}）；\
         請確認 image 內含該命令，或在 appcipe 的 cmd 指定完整路徑"
    )
}
