//! busybox 風格的 applet 派發：當本程式以 `mount` / `umount` 名稱被呼叫時，
//! 提供最小但正確的 mount(8)/umount(8) 實作。
//!
//! 背景：WSL2 的 init 在 distro 啟動時會執行 distro 內的 `/bin/mount` 來掛載
//! drvfs（9p）等檔案系統（實測呼叫形如：
//! `mount -i -t 9p C:\ /mnt/c -o msize=65536,trans=fd,rfdno=5,wfdno=5,cache=mmap,noatime,aname=drvfs;path=C:\;uid=0;gid=0;symlinkroot=/mnt/`，
//! 9p 連線經由繼承的 fd 傳遞）。chefer 的最小 rootfs 不帶 busybox，
//! 因此把 mount/umount 以 symlink 指到 guest-agent，由本模組處理。
//!
//! 解析部分為跨平台純函式（可在任何平台測試）；實際 mount(2)/umount(2)
//! 僅在 Linux 編譯。

/// 已解析的 mount 呼叫。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRequest {
    pub source: Option<String>,
    pub target: Option<String>,
    pub fstype: Option<String>,
    /// 旗標 token（ro/noatime/bind/...，已自 -o 字串分離）
    pub flag_tokens: Vec<String>,
    /// 交給檔案系統的 data 字串（非旗標 token 以逗號重組）
    pub data: String,
    /// `mount -a`
    pub mount_all: bool,
    /// 無參數的 `mount`（列出掛載）
    pub list_only: bool,
}

/// 解析 mount argv（不含 argv[0]）。支援：`-i`/`-n`/`-f`/`-v`（忽略）、
/// `-t <type>`、`-o <opts>`（可重複，逗號累加）、`-a`、`--`、兩個位置參數。
pub fn parse_mount_args<I: IntoIterator<Item = String>>(args: I) -> Result<MountRequest, String> {
    let mut it = args.into_iter();
    let mut req = MountRequest {
        source: None,
        target: None,
        fstype: None,
        flag_tokens: vec![],
        data: String::new(),
        mount_all: false,
        list_only: false,
    };
    let mut opt_strings: Vec<String> = vec![];
    let mut positionals: Vec<String> = vec![];
    let mut no_more_flags = false;

    while let Some(a) = it.next() {
        if no_more_flags || !a.starts_with('-') || a == "-" {
            positionals.push(a);
            continue;
        }
        match a.as_str() {
            "--" => no_more_flags = true,
            "-a" => req.mount_all = true,
            // -i：不呼叫 mount.<type> helper（我們本來就不會）；
            // -n：不寫 mtab；-f：fake；-v：verbose——皆可安全忽略
            "-i" | "-n" | "-f" | "-v" => {}
            "-t" => {
                let v = it.next().ok_or("-t 需要參數")?;
                req.fstype = Some(v);
            }
            "-o" => {
                let v = it.next().ok_or("-o 需要參數")?;
                opt_strings.push(v);
            }
            "-r" => opt_strings.push("ro".into()),
            "-w" => opt_strings.push("rw".into()),
            other => return Err(format!("不支援的 mount 參數：{other}")),
        }
    }

    if positionals.len() > 2 {
        return Err(format!("過多位置參數：{positionals:?}"));
    }
    let mut pos = positionals.into_iter();
    match (pos.next(), pos.next()) {
        (Some(a), Some(b)) => {
            req.source = Some(a);
            req.target = Some(b);
        }
        (Some(a), None) => req.target = Some(a), // `mount /dir`（fstab 形式）——少見
        (None, _) => {}
    }

    // 切開 -o：旗標 token 與 data token 分流
    let mut data_tokens: Vec<String> = vec![];
    for s in opt_strings {
        for tok in s.split(',') {
            if tok.is_empty() {
                continue;
            }
            if is_flag_token(tok) {
                req.flag_tokens.push(tok.to_string());
            } else {
                data_tokens.push(tok.to_string());
            }
        }
    }
    req.data = data_tokens.join(",");

    if !req.mount_all && req.source.is_none() && req.target.is_none() {
        req.list_only = true;
    }
    Ok(req)
}

/// 是否為對應 MS_* 旗標的 token（其餘進 data 交給檔案系統）。
fn is_flag_token(tok: &str) -> bool {
    matches!(
        tok,
        "ro" | "rw"
            | "nosuid"
            | "suid"
            | "nodev"
            | "dev"
            | "noexec"
            | "exec"
            | "sync"
            | "async"
            | "dirsync"
            | "remount"
            | "bind"
            | "rbind"
            | "noatime"
            | "atime"
            | "nodiratime"
            | "diratime"
            | "relatime"
            | "norelatime"
            | "strictatime"
            | "nostrictatime"
            | "lazytime"
            | "nolazytime"
            | "silent"
            | "loud"
            | "defaults"
    )
}

/// 已解析的 umount 呼叫。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmountRequest {
    pub targets: Vec<String>,
    pub force: bool,
    pub lazy: bool,
    pub umount_all: bool,
}

/// 解析 umount argv（不含 argv[0]）：`-f`、`-l`、`-n`（忽略）、`-a`、目標列表。
pub fn parse_umount_args<I: IntoIterator<Item = String>>(args: I) -> Result<UmountRequest, String> {
    let mut req = UmountRequest {
        targets: vec![],
        force: false,
        lazy: false,
        umount_all: false,
    };
    for a in args {
        match a.as_str() {
            "-f" => req.force = true,
            "-l" => req.lazy = true,
            "-a" => req.umount_all = true,
            "-n" | "-v" | "-i" | "-d" | "-r" => {}
            "--" => {}
            other if other.starts_with('-') => {
                return Err(format!("不支援的 umount 參數：{other}"));
            }
            target => req.targets.push(target.to_string()),
        }
    }
    Ok(req)
}

/// 由 argv[0] 判斷 applet 名稱（basename，去掉路徑）。
pub fn applet_name(argv0: &str) -> Option<&'static str> {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    match base {
        "mount" => Some("mount"),
        "umount" => Some("umount"),
        _ => None,
    }
}

/// applet 進入點：argv[0] 是 mount/umount 時執行對應 applet 並回傳 exit code；
/// 否則回 None 讓正常 CLI 接手。
pub fn maybe_run_applet() -> Option<i32> {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let name = applet_name(&argv0)?;
    let rest: Vec<String> = args.collect();
    Some(match name {
        "mount" => run_mount(rest),
        "umount" => run_umount(rest),
        _ => unreachable!(),
    })
}

fn run_mount(args: Vec<String>) -> i32 {
    let req = match parse_mount_args(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mount: {e}");
            return 1;
        }
    };
    do_mount(&req)
}

fn run_umount(args: Vec<String>) -> i32 {
    let req = match parse_umount_args(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("umount: {e}");
            return 1;
        }
    };
    do_umount(&req)
}

#[cfg(target_os = "linux")]
fn do_mount(req: &MountRequest) -> i32 {
    use nix::mount::{MsFlags, mount};

    if req.list_only {
        // `mount`（無參數）：印出目前掛載
        match std::fs::read_to_string("/proc/mounts") {
            Ok(s) => {
                print!("{s}");
                return 0;
            }
            Err(e) => {
                eprintln!("mount: 無法讀取 /proc/mounts：{e}");
                return 1;
            }
        }
    }
    if req.mount_all {
        // mountFsTab=false 的環境下不應被呼叫；無 fstab 視為成功
        return 0;
    }
    let (Some(source), Some(target)) = (&req.source, &req.target) else {
        eprintln!("mount: 缺少 source/target");
        return 1;
    };

    let mut flags = MsFlags::empty();
    for tok in &req.flag_tokens {
        match tok.as_str() {
            "ro" => flags |= MsFlags::MS_RDONLY,
            "rw" => flags &= !MsFlags::MS_RDONLY,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
            "dirsync" => flags |= MsFlags::MS_DIRSYNC,
            "remount" => flags |= MsFlags::MS_REMOUNT,
            "bind" => flags |= MsFlags::MS_BIND,
            "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
            "noatime" => flags |= MsFlags::MS_NOATIME,
            "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
            "relatime" => flags |= MsFlags::MS_RELATIME,
            "strictatime" => flags |= MsFlags::MS_STRICTATIME,
            "lazytime" => flags |= MsFlags::MS_LAZYTIME,
            "silent" => flags |= MsFlags::MS_SILENT,
            // suid/dev/exec/async/atime/defaults 等為預設行為（清旗標），略過
            _ => {}
        }
    }

    let data = if req.data.is_empty() {
        None
    } else {
        Some(req.data.as_str())
    };
    let res = mount(
        Some(source.as_str()),
        target.as_str(),
        req.fstype.as_deref(),
        flags,
        data,
    );
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!(
                "mount: 掛載 {source} 到 {target}（type={:?}）失敗：{e}",
                req.fstype
            );
            32 // util-linux mount 的失敗碼慣例
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn do_mount(_req: &MountRequest) -> i32 {
    eprintln!("mount: 僅支援 Linux");
    1
}

#[cfg(target_os = "linux")]
fn do_umount(req: &UmountRequest) -> i32 {
    use nix::mount::{MntFlags, umount2};

    if req.umount_all {
        // 盡力而為：關機時 init 自會處理，無需逐一卸載
        return 0;
    }
    let mut flags = MntFlags::empty();
    if req.force {
        flags |= MntFlags::MNT_FORCE;
    }
    if req.lazy {
        flags |= MntFlags::MNT_DETACH;
    }
    let mut code = 0;
    for t in &req.targets {
        if let Err(e) = umount2(t.as_str(), flags) {
            eprintln!("umount: 卸載 {t} 失敗：{e}");
            code = 32;
        }
    }
    code
}

#[cfg(not(target_os = "linux"))]
fn do_umount(_req: &UmountRequest) -> i32 {
    eprintln!("umount: 僅支援 Linux");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// WSL init 實測的 drvfs 掛載呼叫必須完整解析正確。
    #[test]
    fn parses_wsl_drvfs_invocation() {
        let req = parse_mount_args(v(&[
            "-i",
            "-t",
            "9p",
            r"C:\",
            "/mnt/c",
            "-o",
            r"msize=65536,trans=fd,rfdno=5,wfdno=5,cache=mmap,noatime,aname=drvfs;path=C:\;uid=0;gid=0;metadata;symlinkroot=/mnt/",
        ]))
        .unwrap();
        assert_eq!(req.source.as_deref(), Some(r"C:\"));
        assert_eq!(req.target.as_deref(), Some("/mnt/c"));
        assert_eq!(req.fstype.as_deref(), Some("9p"));
        assert_eq!(req.flag_tokens, vec!["noatime"]);
        assert_eq!(
            req.data,
            r"msize=65536,trans=fd,rfdno=5,wfdno=5,cache=mmap,aname=drvfs;path=C:\;uid=0;gid=0;metadata;symlinkroot=/mnt/"
        );
        assert!(!req.mount_all);
        assert!(!req.list_only);
    }

    #[test]
    fn parses_bind_and_flags() {
        let req = parse_mount_args(v(&["-o", "bind,ro", "/a", "/b"])).unwrap();
        assert_eq!(req.flag_tokens, vec!["bind", "ro"]);
        assert_eq!(req.data, "");
    }

    #[test]
    fn repeated_o_accumulates() {
        let req =
            parse_mount_args(v(&["-o", "noatime", "-o", "size=64m", "tmpfs", "/tmp"])).unwrap();
        assert_eq!(req.flag_tokens, vec!["noatime"]);
        assert_eq!(req.data, "size=64m");
    }

    #[test]
    fn no_args_is_list_only() {
        let req = parse_mount_args(v(&[])).unwrap();
        assert!(req.list_only);
    }

    #[test]
    fn mount_all_flag() {
        let req = parse_mount_args(v(&["-a"])).unwrap();
        assert!(req.mount_all);
        assert!(!req.list_only);
    }

    #[test]
    fn unknown_flag_is_error() {
        assert!(parse_mount_args(v(&["--make-rprivate", "/"])).is_err());
    }

    #[test]
    fn parses_umount() {
        let req = parse_umount_args(v(&["-l", "/mnt/c"])).unwrap();
        assert!(req.lazy);
        assert!(!req.force);
        assert_eq!(req.targets, vec!["/mnt/c"]);
    }

    #[test]
    fn applet_name_dispatch() {
        assert_eq!(applet_name("/bin/mount"), Some("mount"));
        assert_eq!(applet_name("umount"), Some("umount"));
        assert_eq!(applet_name("/bin/guest-agent"), None);
        assert_eq!(applet_name(r"C:\x\mount"), Some("mount"));
    }
}
