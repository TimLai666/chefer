//! WSL2 後端使用的純函式：Windows→WSL 路徑轉換、distro 命名、
//! 最小 rootfs tar 產生、wsl.exe 輸出解碼。
//!
//! 全部與作業系統無關，跨平台皆可編譯與測試。

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

/// chefer 專用 WSL distro 的名稱前綴。
pub const DISTRO_PREFIX: &str = "chefer-rt-";

/// `/etc/wsl.conf` 內容：啟用 automount（讓 /mnt/<drive> 可用）並開 metadata
/// （drvfs 上支援 chmod/chown/symlink，persist 目錄需要）、不啟動 systemd。
pub const WSL_CONF: &str =
    "[automount]\nenabled=true\nmountFsTab=false\noptions=\"metadata\"\n[boot]\nsystemd=false\n";

/// `/etc/passwd` 內容：僅 root 一個使用者。
pub const ETC_PASSWD: &str = "root:x:0:0:root:/root:/bin/false\n";

/// 把 Windows 絕對路徑轉成 WSL 路徑（`C:\foo\bar` → `/mnt/c/foo/bar`）。
///
/// 規則：磁碟代號轉小寫、反斜線轉正斜線；接受 `\\?\C:\...` verbatim 前綴；
/// UNC 網路路徑（`\\server\share`、`\\?\UNC\...`）與裝置路徑（`\\.\...`）一律報錯。
pub fn windows_path_to_wsl(path: &str) -> Result<String> {
    // verbatim 前綴 \\?\：後接磁碟代號則剝掉前綴，後接 UNC 則屬網路路徑
    let s = if let Some(rest) = path.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            rest
        } else {
            bail!(
                "UNC network paths and device paths are not supported: {path}; put the file on a local drive (e.g. C:\\...) and try again"
            );
        }
    } else if path.starts_with(r"\\") || path.starts_with("//") {
        bail!(
            "UNC network paths and device paths are not supported: {path}; put the file on a local drive (e.g. C:\\...) and try again"
        );
    } else {
        path
    };

    let b = s.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        bail!(
            "cannot convert to a WSL path (an absolute Windows path is required, e.g. C:\\foo\\bar): {path}"
        );
    }
    let drive = (b[0] as char).to_ascii_lowercase();
    let rest = &s[2..];
    if !(rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/')) {
        bail!(
            "cannot convert a drive-relative path (e.g. C:foo) to a WSL path: {path}; provide a full absolute path (e.g. C:\\foo)"
        );
    }
    let rest = rest.replace('\\', "/");
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        Ok(format!("/mnt/{drive}"))
    } else {
        Ok(format!("/mnt/{drive}/{rest}"))
    }
}

/// 由 guest-agent 二進位內容算出 distro 名：`chefer-rt-<sha256 前 8 碼>`。
/// 同一份 agent 永遠對應同一個 distro（冪等、可重用）。
pub fn agent_distro_name(agent_bytes: &[u8]) -> String {
    let digest = Sha256::digest(agent_bytes);
    // 取前 4 bytes → 8 個 hex 字元
    let hash8: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{DISTRO_PREFIX}{hash8}")
}

/// 解碼 wsl.exe 的輸出。設了 `WSL_UTF8=1` 後新版 wsl.exe 會輸出 UTF-8，
/// 但舊版可能仍輸出 UTF-16LE（含 NUL 位元組）——偵測到 NUL 時改以 UTF-16LE 解碼。
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.contains(&0) {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 在記憶體產生 `wsl --import` 用的最小 rootfs tar：
/// 標準 FHS 目錄（含 /mnt、/sys、/usr、/run——WSL init 的 automount 與
/// interop 需要這些掛載點存在，缺 /mnt 會導致 /mnt/<drive> 掛載失敗）、
/// 檔案 /bin/guest-agent（mode 0o755）、/etc/wsl.conf、/etc/passwd。
pub fn build_min_rootfs_tar(agent_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());

    for dir in [
        "bin/", "proc/", "tmp/", "etc/", "dev/", "root/", "var/", "mnt/", "sys/", "usr/", "run/",
        "home/",
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(0);
        builder.append_data(&mut h, dir, std::io::empty())?;
    }

    append_file(&mut builder, "bin/guest-agent", agent_bytes, 0o755)?;
    append_file(&mut builder, "etc/wsl.conf", WSL_CONF.as_bytes(), 0o644)?;
    append_file(&mut builder, "etc/passwd", ETC_PASSWD.as_bytes(), 0o644)?;
    append_symlink(&mut builder, "tmp/.X11-unix", "/mnt/wslg/.X11-unix")?;

    // WSL init 啟動 distro 時會執行 distro 內的 /bin/mount 來掛載 drvfs（/mnt/c），
    // 缺少時 automount 與 interop 整個失效——以 symlink 指向 guest-agent，
    // 由其 applet 模式（argv[0] 派發）提供 mount/umount 實作。
    for applet in ["mount", "umount"] {
        append_symlink(&mut builder, &format!("bin/{applet}"), "guest-agent")?;
    }

    Ok(builder.into_inner()?)
}

/// 在 tar 內加入 symlink entry。
fn append_symlink(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    link_target: &str,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_mode(0o777);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_size(0);
    builder.append_link(&mut h, path, link_target)?;
    Ok(())
}

fn append_file(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    content: &[u8],
    mode: u32,
) -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_size(content.len() as u64);
    builder.append_data(&mut h, path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read;

    // ---- windows_path_to_wsl ----

    #[test]
    fn path_basic_backslash() {
        assert_eq!(
            windows_path_to_wsl(r"C:\foo\bar").unwrap(),
            "/mnt/c/foo/bar"
        );
    }

    #[test]
    fn path_lowercases_drive_and_accepts_forward_slash() {
        assert_eq!(windows_path_to_wsl("D:/x/y").unwrap(), "/mnt/d/x/y");
        assert_eq!(windows_path_to_wsl(r"Z:\a").unwrap(), "/mnt/z/a");
    }

    #[test]
    fn path_drive_root_and_trailing_slash() {
        assert_eq!(windows_path_to_wsl(r"c:\").unwrap(), "/mnt/c");
        assert_eq!(windows_path_to_wsl("C:").unwrap(), "/mnt/c");
        assert_eq!(windows_path_to_wsl(r"C:\foo\").unwrap(), "/mnt/c/foo");
    }

    #[test]
    fn path_preserves_spaces_and_unicode() {
        assert_eq!(
            windows_path_to_wsl(r"C:\Program Files\應用 程式").unwrap(),
            "/mnt/c/Program Files/應用 程式"
        );
    }

    #[test]
    fn path_verbatim_prefix_is_stripped() {
        assert_eq!(
            windows_path_to_wsl(r"\\?\C:\foo\bar").unwrap(),
            "/mnt/c/foo/bar"
        );
    }

    #[test]
    fn path_unc_is_rejected() {
        for p in [
            r"\\server\share\x",
            r"\\?\UNC\server\share",
            r"\\.\PhysicalDrive0",
            "//server/share",
        ] {
            let err = windows_path_to_wsl(p).unwrap_err();
            assert!(
                format!("{err}").contains("UNC"),
                "應拒絕 UNC/裝置路徑 {p}：{err}"
            );
        }
    }

    #[test]
    fn path_relative_is_rejected() {
        assert!(windows_path_to_wsl(r"relative\path").is_err());
        assert!(windows_path_to_wsl("/unix/style").is_err());
        assert!(windows_path_to_wsl("C:relative").is_err()); // 磁碟相對路徑
        assert!(windows_path_to_wsl("").is_err());
    }

    // ---- agent_distro_name ----

    #[test]
    fn distro_name_is_deterministic_and_well_formed() {
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(agent_distro_name(b"hello"), "chefer-rt-2cf24dba");
        // 冪等：同輸入同名
        assert_eq!(agent_distro_name(b"hello"), agent_distro_name(b"hello"));
        // 不同輸入不同名
        assert_ne!(agent_distro_name(b"hello"), agent_distro_name(b"world"));
        let name = agent_distro_name(b"abc");
        assert!(name.starts_with(DISTRO_PREFIX));
        assert_eq!(name.len(), DISTRO_PREFIX.len() + 8);
    }

    // ---- decode_wsl_output ----

    #[test]
    fn decode_utf8_and_utf16le() {
        assert_eq!(decode_wsl_output("abc\n".as_bytes()), "abc\n");
        // "ab\n" 的 UTF-16LE 編碼
        let utf16: Vec<u8> = vec![0x61, 0x00, 0x62, 0x00, 0x0a, 0x00];
        assert_eq!(decode_wsl_output(&utf16), "ab\n");
    }

    // ---- build_min_rootfs_tar ----

    #[test]
    fn rootfs_tar_contains_expected_entries() {
        let agent = b"#!/fake-agent-binary".to_vec();
        let tar_bytes = build_min_rootfs_tar(&agent).unwrap();

        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut dirs: Vec<String> = Vec::new();
        let mut files: BTreeMap<String, (u32, Vec<u8>)> = BTreeMap::new();
        let mut symlinks: BTreeMap<String, String> = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string();
            let mode = entry.header().mode().unwrap();
            match entry.header().entry_type() {
                tar::EntryType::Directory => {
                    assert_eq!(mode, 0o755, "目錄 {path} 的 mode 應為 0755");
                    dirs.push(path);
                }
                tar::EntryType::Regular => {
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).unwrap();
                    files.insert(path, (mode, buf));
                }
                tar::EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .unwrap()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    symlinks.insert(path, target);
                }
                other => panic!("rootfs tar 不應含 {other:?} 類型的 entry"),
            }
        }

        for d in [
            "bin", "proc", "tmp", "etc", "dev", "root", "var", "mnt", "sys", "usr", "run", "home",
        ] {
            assert!(dirs.iter().any(|p| p == d), "缺少目錄 {d}（有：{dirs:?}）");
        }

        // WSL init 需要 distro 內的 mount/umount（automount 用）
        assert_eq!(symlinks["bin/mount"], "guest-agent");
        assert_eq!(symlinks["bin/umount"], "guest-agent");
        assert_eq!(symlinks["tmp/.X11-unix"], "/mnt/wslg/.X11-unix");

        let (mode, content) = &files["bin/guest-agent"];
        assert_eq!(*mode, 0o755, "/bin/guest-agent 必須可執行");
        assert_eq!(
            content, &agent,
            "/bin/guest-agent 內容應與 agent bytes 相同"
        );

        let (_, conf) = &files["etc/wsl.conf"];
        assert_eq!(String::from_utf8_lossy(conf), WSL_CONF);

        let (_, passwd) = &files["etc/passwd"];
        assert_eq!(String::from_utf8_lossy(passwd), ETC_PASSWD);
    }
}
