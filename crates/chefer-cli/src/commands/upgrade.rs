//! `chefer upgrade` — 從 GitHub Releases 更新完整 runtime kit（不依賴 cargo）。
//!
//! release workflow 上傳的是 `chefer_<tag>_<target>.zip|tar.gz`，內含 host CLI
//! 與完整 `kit/`；因此不能使用 self_update 的單一 binary 替換流程。

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use owo_colors::OwoColorize;
use reqwest::header;
use self_update::cargo_crate_version;
use self_update::update::{Release, ReleaseAsset};
use sha2::{Digest, Sha256};

use crate::{BIN_NAME, HOST_TARGET, REPO_NAME, REPO_OWNER};

pub fn cmd_upgrade(channel: &str, to: Option<&str>, check_only: bool) -> Result<()> {
    let current = cargo_crate_version!();
    let release = fetch_release(channel, to)?;
    let archive = select_kit_asset(&release, HOST_TARGET)?;
    let checksum = select_checksum_asset(&release, &archive.name)?;
    let newer = self_update::version::bump_is_greater(current, &release.version)
        .unwrap_or(release.version != current);

    if check_only {
        if newer {
            println!(
                "{}  {} → {} ({})",
                "Update available".yellow().bold(),
                current,
                release.version,
                archive.name
            );
        } else {
            println!(
                "{}  {} ({})",
                "Already up to date".green().bold(),
                current,
                archive.name
            );
        }
        return Ok(());
    }
    if !should_install_release(channel, to, newer) {
        println!(
            "{}  {} ({})",
            "Already up to date".green().bold(),
            current,
            archive.name
        );
        return Ok(());
    }

    let exe = std::env::current_exe().context("failed to get current chefer executable path")?;
    let install_dir = exe.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine chefer install directory: {}; please download the release kit manually and overwrite the install directory",
            exe.display()
        )
    })?;

    let work = tempfile::Builder::new()
        .prefix("chefer-upgrade-")
        .tempdir_in(install_dir)
        .with_context(|| {
            format!(
                "failed to create temp folder in install directory: {}",
                install_dir.display()
            )
        })?;
    let archive_path = work.path().join(&archive.name);
    let checksum_path = work.path().join(&checksum.name);

    println!(
        "{}  {} ({})",
        "Downloading".yellow().bold(),
        release.version,
        archive.name
    );
    download_asset(&archive, &archive_path)
        .with_context(|| format!("failed to download release kit: {}", archive.name))?;
    download_asset(&checksum, &checksum_path)
        .with_context(|| format!("failed to download checksum: {}", checksum.name))?;

    let expected = parse_sha256_file(&checksum_path)
        .with_context(|| format!("failed to parse checksum file: {}", checksum.name))?;
    let got = file_sha256_hex(&archive_path).with_context(|| {
        format!(
            "failed to compute release kit SHA-256: {}",
            archive_path.display()
        )
    })?;
    if got != expected {
        bail!(
            "release kit SHA-256 mismatch:\n  expected: {expected}\n  actual:   {got}\n\
             update aborted; please re-run upgrade, or download the release kit manually and compare against .sha256"
        );
    }

    let extract_dir = work.path().join("extract");
    fs::create_dir_all(&extract_dir).with_context(|| {
        format!(
            "failed to create extract directory: {}",
            extract_dir.display()
        )
    })?;
    extract_kit_archive(&archive_path, &extract_dir)
        .with_context(|| format!("failed to extract release kit: {}", archive.name))?;

    let root = validate_extracted_kit(&extract_dir, &archive.name, HOST_TARGET)?;
    let new_exe = root.join(host_bin_name(HOST_TARGET));
    let new_kit = root.join("kit");

    println!("{}", "Installing".yellow().bold());
    self_replace::self_replace(&new_exe).with_context(|| {
        format!(
            "failed to replace current chefer executable; you can extract {} manually and copy {} to {}",
            archive.name,
            new_exe.display(),
            exe.display()
        )
    })?;
    replace_kit_dir(install_dir, &new_kit).with_context(|| {
        format!(
            "failed to replace kit/; the chefer binary may already be updated. Please extract {} manually and overwrite {}",
            archive.name,
            install_dir.join("kit").display()
        )
    })?;

    println!(
        "{}  {} → {}",
        "✔ Updated".green().bold(),
        current,
        release.version
    );
    println!(
        "{}",
        "Note: verified against the .sha256 shipped with the Release; the maintainer signature has not been verified yet.".dimmed()
    );
    Ok(())
}

fn fetch_release(channel: &str, to: Option<&str>) -> Result<Release> {
    use self_update::backends::github::Update;

    let mut builder = Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .show_download_progress(true)
        .current_version(cargo_crate_version!());

    let updater = builder.build()?;
    if let Some(ver) = to {
        return updater
            .get_release_version(ver)
            .with_context(|| format!("failed to look up GitHub Release tag `{ver}`"));
    }
    if channel != "stable" {
        return updater
            .get_release_version(channel)
            .with_context(|| format!("failed to look up GitHub Release tag `{channel}`"));
    }
    updater
        .get_latest_release()
        .context("failed to look up the latest GitHub Release")
}

fn select_kit_asset(release: &Release, target: &str) -> Result<ReleaseAsset> {
    let suffix = kit_archive_suffix(target);
    let target_suffix = format!("_{target}{suffix}");
    let matches: Vec<ReleaseAsset> = release
        .assets
        .iter()
        .filter(|a| {
            is_safe_asset_filename(&a.name)
                && a.name.starts_with("chefer_")
                && a.name.ends_with(&target_suffix)
                && !a.name.ends_with(".sha256")
        })
        .cloned()
        .collect();
    match matches.as_slice() {
        [asset] => Ok(asset.clone()),
        [] => bail!(
            "No kit archive for this platform `{target}` in release `{}` \
             (expected a file name ending in `{suffix}`).\n\
             If this release was just published, its assets may still be uploading — \
             please wait a few minutes and try again. \
             If it persists, check that the release workflow uploaded kits for all targets.",
            release.version
        ),
        many => bail!(
            "Release `{}` has multiple kit archives matching `{target}`; \
             cannot choose safely: {}",
            release.version,
            many.iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn select_checksum_asset(release: &Release, archive_name: &str) -> Result<ReleaseAsset> {
    let checksum_name = format!("{archive_name}.sha256");
    release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Release `{}` has no checksum file `{checksum_name}`; \
                 upgrade must verify against .sha256 before extracting and installing",
                release.version
            )
        })
}

fn kit_archive_suffix(target: &str) -> &'static str {
    if target.contains("windows") {
        ".zip"
    } else {
        ".tar.gz"
    }
}

fn host_bin_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "chefer.exe"
    } else {
        "chefer"
    }
}

fn should_install_release(channel: &str, to: Option<&str>, newer: bool) -> bool {
    newer || to.is_some() || channel != "stable"
}

fn is_safe_asset_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
}

fn download_asset(asset: &ReleaseAsset, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::ClientBuilder::new()
        .use_rustls_tls()
        .build()
        .context("failed to build HTTPS client")?;
    let mut resp = client
        .get(&asset.download_url)
        .header(header::USER_AGENT, "chefer-upgrade")
        .header(header::ACCEPT, "application/octet-stream")
        .send()
        .with_context(|| format!("failed to send download request: {}", asset.download_url))?;
    if !resp.status().is_success() {
        bail!(
            "failed to download `{}` (HTTP {}): {}",
            asset.name,
            resp.status(),
            asset.download_url
        );
    }
    let mut out = File::create(dest)
        .with_context(|| format!("failed to create download file: {}", dest.display()))?;
    io::copy(&mut resp, &mut out)
        .with_context(|| format!("failed to write download file: {}", dest.display()))?;
    Ok(())
}

fn parse_sha256_file(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read checksum file: {}", path.display()))?;
    let first = text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("checksum file is empty"))?;
    if first.len() != 64 || !first.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("first checksum field is not a 64-digit hex SHA-256: {first}");
    }
    Ok(first.to_ascii_lowercase())
}

fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut f =
        File::open(path).with_context(|| format!("failed to open file: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("failed to read file: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn extract_kit_archive(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else if name.ends_with(".tar.gz") {
        extract_tar_gz(archive, dest)
    } else {
        bail!(
            "unsupported release kit archive format: {}",
            archive.display()
        )
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let f = File::open(archive)
        .with_context(|| format!("failed to open tar.gz: {}", archive.display()))?;
    let gz = GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    for entry in ar.entries().context("failed to read tar.gz contents")? {
        let mut entry = entry.context("failed to read tar entry")?;
        let raw = entry
            .path()
            .context("failed to parse tar entry path")?
            .into_owned();
        let rel = sanitize_archive_path(&raw)?;
        let out = dest.join(&rel);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&out)
                    .with_context(|| format!("failed to create directory: {}", out.display()))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create directory: {}", parent.display())
                    })?;
                }
                let mut file = File::create(&out)
                    .with_context(|| format!("failed to create file: {}", out.display()))?;
                io::copy(&mut entry, &mut file)
                    .with_context(|| format!("failed to write file: {}", out.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = entry.header().mode() {
                        let _ = fs::set_permissions(&out, fs::Permissions::from_mode(mode));
                    }
                }
            }
            other => bail!(
                "release kit tar contains an unsupported entry type {:?}: {}",
                other,
                raw.display()
            ),
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let f = File::open(archive)
        .with_context(|| format!("failed to open zip: {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f).context("failed to read zip contents")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("failed to read zip entry")?;
        let rel = sanitize_archive_name(entry.name())?;
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)
                .with_context(|| format!("failed to create directory: {}", out.display()))?;
            continue;
        }
        if entry.is_symlink() {
            bail!(
                "release kit zip contains an unsupported symlink: {}",
                entry.name()
            );
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 {
                bail!(
                    "release kit zip contains an unsupported entry mode {mode:o}: {}",
                    entry.name()
                );
            }
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let mut file = File::create(&out)
            .with_context(|| format!("failed to create file: {}", out.display()))?;
        io::copy(&mut entry, &mut file)
            .with_context(|| format!("failed to write file: {}", out.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn sanitize_archive_name(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.contains('\\') {
        bail!("release kit archive contains an unsafe path: {raw}");
    }
    sanitize_archive_path(Path::new(raw))
}

fn sanitize_archive_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                bail!(
                    "release kit archive contains an unsafe path: {}",
                    path.display()
                );
            }
            Component::CurDir => {}
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains(':') || s.contains('\\') {
                    bail!("release kit archive path segment contains an illegal character: {s}");
                }
                out.push(seg);
            }
        }
    }
    if out.as_os_str().is_empty() {
        bail!("release kit archive contains an empty path");
    }
    Ok(out)
}

fn validate_extracted_kit(extract_dir: &Path, archive_name: &str, target: &str) -> Result<PathBuf> {
    let root_name = archive_root_name(archive_name)?;
    for entry in fs::read_dir(extract_dir).with_context(|| {
        format!(
            "failed to read extract directory: {}",
            extract_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to read extract directory entry: {}",
                extract_dir.display()
            )
        })?;
        if entry.file_name() != OsStr::new(&root_name) {
            bail!(
                "release kit extracted to an unexpected top-level entry `{}`; \
                 the archive must contain only the `{root_name}/` root directory",
                entry.file_name().to_string_lossy()
            );
        }
    }
    let root = extract_dir.join(&root_name);
    if !root.is_dir() {
        bail!(
            "could not find root directory `{root_name}` after extracting the release kit; make sure the archive was produced by the release workflow"
        );
    }
    let bin = root.join(host_bin_name(target));
    if !bin.is_file() {
        bail!("release kit is missing the host CLI: {}", bin.display());
    }
    let kit = root.join("kit");
    if !kit.is_dir() {
        bail!(
            "release kit is missing the kit/ directory: {}",
            kit.display()
        );
    }
    for name in required_kit_files() {
        let path = kit.join(name);
        if !path.is_file() {
            bail!("release kit is missing a required file: {}", path.display());
        }
    }
    Ok(root)
}

fn archive_root_name(archive_name: &str) -> Result<String> {
    if let Some(s) = archive_name.strip_suffix(".tar.gz") {
        return Ok(s.to_string());
    }
    if let Some(s) = archive_name.strip_suffix(".zip") {
        return Ok(s.to_string());
    }
    bail!("cannot derive root directory from archive file name: {archive_name}")
}

fn required_kit_files() -> &'static [&'static str] {
    &[
        "chefer-runtime-x86_64-pc-windows-msvc.exe",
        "chefer-runtime-aarch64-pc-windows-msvc.exe",
        "chefer-runtime-x86_64-unknown-linux-musl",
        "chefer-runtime-aarch64-unknown-linux-musl",
        "chefer-runtime-x86_64-apple-darwin",
        "chefer-runtime-aarch64-apple-darwin",
        "guest-agent-x86_64",
        "guest-agent-aarch64",
    ]
}

fn replace_kit_dir(install_dir: &Path, new_kit: &Path) -> Result<()> {
    let target = install_dir.join("kit");
    let backup = install_dir.join(format!("kit.previous-{}", std::process::id()));
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("failed to clear old kit backup: {}", backup.display()))?;
    }
    if target.exists() {
        fs::rename(&target, &backup).with_context(|| {
            format!(
                "failed to back up existing kit: {} → {}",
                target.display(),
                backup.display()
            )
        })?;
    }
    match fs::rename(new_kit, &target) {
        Ok(()) => {
            if backup.exists() {
                let _ = fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(err) => {
            if backup.exists() && !target.exists() {
                let _ = fs::rename(&backup, &target);
            }
            Err(err).with_context(|| {
                format!(
                    "failed to install new kit: {} → {}",
                    new_kit.display(),
                    target.display()
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            download_url: format!("https://example.invalid/{name}"),
        }
    }

    fn release(assets: Vec<ReleaseAsset>) -> Release {
        Release {
            name: "v-test".into(),
            version: "test".into(),
            date: "2026-01-01T00:00:00Z".into(),
            body: None,
            assets,
        }
    }

    #[test]
    fn selects_host_kit_asset_and_checksum() {
        let rel = release(vec![
            asset("chefer_v1.2.3_x86_64-pc-windows-msvc.zip"),
            asset("chefer_v1.2.3_x86_64-pc-windows-msvc.zip.sha256"),
            asset("chefer_v1.2.3_x86_64-unknown-linux-musl.tar.gz"),
        ]);
        let kit = select_kit_asset(&rel, "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(kit.name, "chefer_v1.2.3_x86_64-pc-windows-msvc.zip");
        let sha = select_checksum_asset(&rel, &kit.name).unwrap();
        assert_eq!(sha.name, "chefer_v1.2.3_x86_64-pc-windows-msvc.zip.sha256");
    }

    #[test]
    fn stable_upgrade_skips_current_release_unless_explicit() {
        assert!(!should_install_release("stable", None, false));
        assert!(should_install_release("stable", None, true));
        assert!(should_install_release("stable", Some("v0.1.0"), false));
        assert!(should_install_release("nightly", None, false));
    }

    #[test]
    fn rejects_ambiguous_or_pathlike_release_assets() {
        let target = "x86_64-pc-windows-msvc";
        let ambiguous = release(vec![
            asset("chefer_v1_x86_64-pc-windows-msvc.zip"),
            asset("chefer_v1-copy_x86_64-pc-windows-msvc.zip"),
        ]);
        assert!(select_kit_asset(&ambiguous, target).is_err());

        let pathlike = release(vec![asset("chefer_v1/_x86_64-pc-windows-msvc.zip")]);
        assert!(select_kit_asset(&pathlike, target).is_err());

        let missing_sha = release(vec![asset("chefer_v1_x86_64-pc-windows-msvc.zip")]);
        let kit = select_kit_asset(&missing_sha, target).unwrap();
        assert!(select_checksum_asset(&missing_sha, &kit.name).is_err());
    }

    #[test]
    fn parses_sha256sum_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("kit.sha256");
        let hex = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        fs::write(&path, format!("{hex}  chefer.zip\n")).unwrap();
        assert_eq!(parse_sha256_file(&path).unwrap(), hex.to_ascii_lowercase());
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        for bad in ["../x", "/abs/x", "C:/x", "a\\b", ""] {
            assert!(
                sanitize_archive_name(bad).is_err(),
                "{bad} should be rejected"
            );
        }
        assert_eq!(
            sanitize_archive_name("chefer_v/kit/guest-agent-x86_64").unwrap(),
            PathBuf::from("chefer_v/kit/guest-agent-x86_64")
        );
    }

    #[test]
    fn rejects_malformed_sha256_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("kit.sha256");
        fs::write(&path, "not-a-sha  chefer.zip\n").unwrap();
        assert!(parse_sha256_file(&path).is_err());

        fs::write(&path, "\n").unwrap();
        assert!(parse_sha256_file(&path).is_err());
    }

    #[test]
    fn validates_required_kit_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("chefer_v1_x86_64-pc-windows-msvc");
        let kit = root.join("kit");
        fs::create_dir_all(&kit).unwrap();
        fs::write(root.join("chefer.exe"), b"bin").unwrap();
        for name in required_kit_files() {
            fs::write(kit.join(name), name.as_bytes()).unwrap();
        }
        let got = validate_extracted_kit(
            tmp.path(),
            "chefer_v1_x86_64-pc-windows-msvc.zip",
            "x86_64-pc-windows-msvc",
        )
        .unwrap();
        assert_eq!(got, root);
    }

    #[test]
    fn extracts_zip_release_kit_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("chefer_v1_x86_64-pc-windows-msvc.zip");
        {
            let file = File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("chefer_v1_x86_64-pc-windows-msvc/chefer.exe", opts)
                .unwrap();
            zip.write_all(b"bin").unwrap();
            zip.start_file(
                "chefer_v1_x86_64-pc-windows-msvc/kit/guest-agent-x86_64",
                opts,
            )
            .unwrap();
            zip.write_all(b"agent").unwrap();
            zip.finish().unwrap();
        }
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        extract_kit_archive(&archive, &out).unwrap();
        assert!(
            out.join("chefer_v1_x86_64-pc-windows-msvc")
                .join("chefer.exe")
                .is_file()
        );
        assert!(
            out.join("chefer_v1_x86_64-pc-windows-msvc")
                .join("kit")
                .join("guest-agent-x86_64")
                .is_file()
        );
    }

    #[test]
    fn rejects_zip_symlink_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("chefer_v1_x86_64-pc-windows-msvc.zip");
        {
            let file = File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.add_symlink("chefer_v1_x86_64-pc-windows-msvc/link", "target", opts)
                .unwrap();
            zip.finish().unwrap();
        }
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        assert!(extract_kit_archive(&archive, &out).is_err());
    }

    #[test]
    fn extracts_tar_gz_release_kit_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp
            .path()
            .join("chefer_v1_x86_64-unknown-linux-musl.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(
                &mut header,
                "chefer_v1_x86_64-unknown-linux-musl/chefer",
                &b"bin"[..],
            )
            .unwrap();
            tar.finish().unwrap();
        }
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        extract_kit_archive(&archive, &out).unwrap();
        assert!(
            out.join("chefer_v1_x86_64-unknown-linux-musl")
                .join("chefer")
                .is_file()
        );
    }

    #[test]
    fn restores_existing_kit_when_new_kit_install_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("install");
        let existing_kit = install.join("kit");
        fs::create_dir_all(&existing_kit).unwrap();
        fs::write(existing_kit.join("marker"), b"old").unwrap();

        let missing_new_kit = tmp.path().join("missing-kit");
        assert!(replace_kit_dir(&install, &missing_new_kit).is_err());

        assert_eq!(
            fs::read(install.join("kit").join("marker")).unwrap(),
            b"old"
        );
        assert!(
            !install
                .join(format!("kit.previous-{}", std::process::id()))
                .exists()
        );
    }
}
