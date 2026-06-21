//! `chefer doctor` — 檢查目前機器是否具備 build/run Chefer app 的基本條件。

use std::path::PathBuf;

use anyhow::{Context, Result};
use comfy_table::{Attribute, Cell, Color, ColumnConstraint, ContentArrangement, Table, Width};
use owo_colors::OwoColorize;

use crate::ui;
use crate::{APPCIPE_SPEC_VERSION, HOST_TARGET};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    Pass,
    Warn,
    Fail,
}

impl DoctorStatus {
    fn as_str(self) -> &'static str {
        match self {
            DoctorStatus::Pass => "PASS",
            DoctorStatus::Warn => "WARN",
            DoctorStatus::Fail => "FAIL",
        }
    }

    fn color(self) -> Color {
        match self {
            DoctorStatus::Pass => Color::Green,
            DoctorStatus::Warn => Color::Yellow,
            DoctorStatus::Fail => Color::Red,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorStatus,
    pub detail: String,
    pub action: String,
}

impl DoctorCheck {
    fn pass(name: impl Into<String>, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Pass,
            detail: detail.into(),
            action: action.into(),
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Warn,
            detail: detail.into(),
            action: action.into(),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Fail,
            detail: detail.into(),
            action: action.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn has_fail(&self) -> bool {
        self.checks
            .iter()
            .any(|c| matches!(c.status, DoctorStatus::Fail))
    }

    pub fn exit_code(&self) -> i32 {
        if self.has_fail() { 1 } else { 0 }
    }
}

pub fn cmd_doctor(extra_kit_dirs: &[PathBuf]) -> Result<i32> {
    let report = build_report(extra_kit_dirs);
    render_report(&report);
    Ok(report.exit_code())
}

fn build_report(extra_kit_dirs: &[PathBuf]) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(check_platform());
    checks.extend(check_backend_prerequisites());
    checks.push(check_dockerfile_builder());
    checks.extend(check_kit(extra_kit_dirs));
    checks.push(check_version_info());
    checks.push(check_default_data_root());
    DoctorReport { checks }
}

fn check_platform() -> DoctorCheck {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    DoctorCheck::pass(
        "OS / architecture",
        format!("{os}/{arch}"),
        "No action needed.",
    )
}

fn check_backend_prerequisites() -> Vec<DoctorCheck> {
    match std::env::consts::OS {
        "windows" => vec![check_wsl_status(), check_whp_status()],
        "linux" => vec![check_linux_userns()],
        "macos" => vec![DoctorCheck::warn(
            "Runtime backend",
            "The macOS VM backend is implemented but still requires real-Mac validation.",
            "Builds can be packaged, but validate runtime execution on the target Mac before shipping.",
        )],
        other => vec![DoctorCheck::fail(
            "Runtime backend",
            format!("Unsupported host OS: {other}."),
            "Use Linux, Windows with WSL2, or macOS.",
        )],
    }
}

#[cfg(target_os = "windows")]
fn check_wsl_status() -> DoctorCheck {
    match std::process::Command::new("wsl.exe")
        .arg("--status")
        .output()
    {
        Ok(out) if out.status.success() => DoctorCheck::pass(
            "WSL2 backend",
            "wsl.exe --status completed successfully.",
            "No action needed.",
        ),
        Ok(out) => DoctorCheck::fail(
            "WSL2 backend",
            format!("wsl.exe --status exited with status {}.", out.status),
            "Install or repair WSL2, then run `wsl.exe --status` again.",
        ),
        Err(e) => DoctorCheck::fail(
            "WSL2 backend",
            format!("Could not run wsl.exe --status: {e}."),
            "Install WSL2 and ensure wsl.exe is available on PATH.",
        ),
    }
}

#[cfg(target_os = "windows")]
fn check_whp_status() -> DoctorCheck {
    match vmm_backend::whp_availability() {
        vmm_backend::Availability::Available => DoctorCheck::pass(
            "WHP backend",
            "Windows Hypervisor Platform backend is available.",
            "No action needed.",
        ),
        vmm_backend::Availability::Unavailable(reason) => DoctorCheck::warn(
            "WHP backend",
            reason,
            "Enable the Windows Hypervisor Platform optional feature and hardware virtualization, then reboot.",
        ),
    }
}

#[cfg(not(target_os = "windows"))]
fn check_whp_status() -> DoctorCheck {
    DoctorCheck::warn(
        "WHP backend",
        "This check only applies on Windows.",
        "No action needed on this host.",
    )
}

#[cfg(not(target_os = "windows"))]
fn check_wsl_status() -> DoctorCheck {
    DoctorCheck::warn(
        "WSL2 backend",
        "This check only applies on Windows.",
        "No action needed on this host.",
    )
}

#[cfg(target_os = "linux")]
fn check_linux_userns() -> DoctorCheck {
    let uid = current_uid();
    match uid {
        Ok(0) => DoctorCheck::pass(
            "Linux namespaces",
            "Running as root; user namespace delegation is not required.",
            "No action needed.",
        ),
        Ok(uid) => classify_linux_userns(
            uid,
            read_sysctl("/proc/sys/kernel/unprivileged_userns_clone").as_deref(),
            read_sysctl("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref(),
        ),
        Err(e) => DoctorCheck::warn(
            "Linux namespaces",
            format!("Could not determine uid: {e}."),
            "Run `id -u` and verify that root or unprivileged user namespaces are available.",
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn check_linux_userns() -> DoctorCheck {
    DoctorCheck::warn(
        "Linux namespaces",
        "This check only applies on Linux.",
        "No action needed on this host.",
    )
}

#[cfg(target_os = "linux")]
fn current_uid() -> Result<u32> {
    let out = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("failed to execute id -u")?;
    if !out.status.success() {
        anyhow::bail!("id -u exited with status {}", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<u32>()
        .context("id -u did not print a numeric uid")
}

#[cfg(target_os = "linux")]
fn read_sysctl(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(any(test, target_os = "linux"))]
fn classify_linux_userns(
    uid: u32,
    unprivileged_userns_clone: Option<&str>,
    apparmor_restrict_unprivileged_userns: Option<&str>,
) -> DoctorCheck {
    if unprivileged_userns_clone == Some("0") {
        return DoctorCheck::fail(
            "Linux namespaces",
            format!("Running as uid {uid}; kernel.unprivileged_userns_clone is disabled."),
            "Enable unprivileged user namespaces or run the app as root.",
        );
    }
    if apparmor_restrict_unprivileged_userns == Some("1") {
        return DoctorCheck::fail(
            "Linux namespaces",
            format!("Running as uid {uid}; AppArmor restricts unprivileged user namespaces."),
            "Set kernel.apparmor_restrict_unprivileged_userns=0 or run on a host that allows user namespaces.",
        );
    }
    if unprivileged_userns_clone == Some("1") || unprivileged_userns_clone.is_none() {
        return DoctorCheck::pass(
            "Linux namespaces",
            format!("Running as uid {uid}; user namespace settings look usable."),
            "No action needed.",
        );
    }
    DoctorCheck::warn(
        "Linux namespaces",
        format!("Running as uid {uid}; could not confidently classify user namespace settings."),
        "Verify /proc/sys/kernel/unprivileged_userns_clone before running rootless apps.",
    )
}

fn check_kit(extra_kit_dirs: &[PathBuf]) -> Vec<DoctorCheck> {
    let mut kit_dirs = extra_kit_dirs.to_vec();
    kit_dirs.extend(chefer_bundle::kit::default_kit_dirs());
    let mut checks = Vec::new();

    let runtime = chefer_bundle::kit::find_runtime(&kit_dirs, HOST_TARGET, true);
    checks.push(match runtime {
        Some(path) => DoctorCheck::pass(
            "Kit: host runtime",
            format!("Found {}.", path.display()),
            "No action needed.",
        ),
        None => DoctorCheck::fail(
            "Kit: host runtime",
            format!(
                "{} was not found.",
                chefer_bundle::kit::runtime_file_name(HOST_TARGET)
            ),
            chefer_bundle::kit::not_found_help(
                &kit_dirs,
                &chefer_bundle::kit::runtime_file_name(HOST_TARGET),
            ),
        ),
    });

    let arch = host_guest_arch();
    checks.push(match arch {
        Some(arch) => check_guest_agent(&kit_dirs, arch, guest_agent_required_on_host()),
        None => DoctorCheck::fail(
            "Kit: guest-agent",
            format!("Unsupported host architecture: {}.", std::env::consts::ARCH),
            "Use x86_64/amd64 or arm64/aarch64 hardware.",
        ),
    });

    if let Some(arch) = arch {
        checks.push(match chefer_bundle::kit::find_pasta(&kit_dirs, arch) {
            Some(path) => DoctorCheck::pass(
                "Kit: pasta",
                format!("Found {}.", path.display()),
                "No action needed.",
            ),
            None => DoctorCheck::warn(
                "Kit: pasta",
                format!("{} was not found.", chefer_bundle::layout::pasta_name(arch)),
                "Bridge networking will degrade to internal networking unless pasta is available.",
            ),
        });
        checks.push(check_appliance(&kit_dirs, arch));
        checks.push(check_vz_helper(&kit_dirs, arch));
        checks.push(check_whp_helper(&kit_dirs, arch));
    }

    checks
}

fn guest_agent_required_on_host() -> bool {
    matches!(std::env::consts::OS, "windows" | "macos")
}

fn host_guest_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

fn check_guest_agent(kit_dirs: &[PathBuf], arch: &str, required: bool) -> DoctorCheck {
    match chefer_bundle::kit::find_guest_agent(kit_dirs, arch) {
        Some(path) => DoctorCheck::pass(
            "Kit: guest-agent",
            format!("Found {}.", path.display()),
            "No action needed.",
        ),
        None if required => DoctorCheck::fail(
            "Kit: guest-agent",
            format!(
                "{} was not found.",
                chefer_bundle::layout::guest_agent_name(arch)
            ),
            "Install a complete kit from the latest release or pass --kit-dir pointing at one.",
        ),
        None => DoctorCheck::warn(
            "Kit: guest-agent",
            format!(
                "{} was not found.",
                chefer_bundle::layout::guest_agent_name(arch)
            ),
            "Linux can run the in-process backend, but packaging Windows/macOS targets needs guest-agent in the kit.",
        ),
    }
}

fn check_appliance(kit_dirs: &[PathBuf], arch: &str) -> DoctorCheck {
    match chefer_bundle::kit::find_appliance(kit_dirs, arch) {
        Some((kernel, initramfs)) => DoctorCheck::pass(
            "Kit: macOS appliance",
            format!("Found {} and {}.", kernel.display(), initramfs.display()),
            "No action needed.",
        ),
        None => DoctorCheck::warn(
            "Kit: macOS appliance",
            format!(
                "{} or {} was not found.",
                chefer_bundle::layout::kernel_name(arch),
                chefer_bundle::layout::initramfs_name(arch)
            ),
            "Packaging/running macOS VM targets needs the appliance from the latest release kit.",
        ),
    }
}

fn check_vz_helper(kit_dirs: &[PathBuf], arch: &str) -> DoctorCheck {
    match chefer_bundle::kit::find_vz_helper(kit_dirs, arch) {
        Some(path) => DoctorCheck::pass(
            "Kit: VZ helper",
            format!("Found {}.", path.display()),
            "No action needed.",
        ),
        None => DoctorCheck::warn(
            "Kit: VZ helper",
            format!(
                "{} was not found.",
                chefer_bundle::layout::vz_helper_name(arch)
            ),
            "macOS VM execution needs the VZ helper from the latest release kit.",
        ),
    }
}

fn check_whp_helper(kit_dirs: &[PathBuf], arch: &str) -> DoctorCheck {
    match chefer_bundle::kit::find_whp_helper(kit_dirs, arch) {
        Some(path) => {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1);
            let preflight = vmm_backend::whp_preflight_with_helper(&path, cpus);
            DoctorCheck::pass(
                "Kit: WHP helper",
                format!("Found {}. Preflight: {}", path.display(), preflight),
                "No action needed.",
            )
        }
        None => DoctorCheck::warn(
            "Kit: WHP helper",
            format!(
                "{} was not found.",
                chefer_bundle::layout::whp_helper_name(arch)
            ),
            "Windows without WSL2 will need the WHP helper after the VM boot shim is implemented.",
        ),
    }
}

fn check_dockerfile_builder() -> DoctorCheck {
    match chefer_pack::available_builder() {
        Some(builder) => DoctorCheck::pass(
            "Dockerfile builder",
            format!("Found `{builder}` on PATH."),
            "No action needed.",
        ),
        None => DoctorCheck::warn(
            "Dockerfile builder",
            "No container builder (docker/podman/nerdctl/container) found on PATH.",
            "Install one to use `source: dockerfile` in appcipe.yml. \
             `source: tar` and `source: image` work without a builder.",
        ),
    }
}

fn check_version_info() -> DoctorCheck {
    DoctorCheck::pass(
        "Chefer build info",
        format!(
            "chefer {}, AppCipe spec {}, target {}, built {}.",
            env!("CARGO_PKG_VERSION"),
            APPCIPE_SPEC_VERSION,
            HOST_TARGET,
            env!("BUILD_TIME")
        ),
        "No action needed.",
    )
}

fn check_default_data_root() -> DoctorCheck {
    match default_data_root() {
        Ok(root) => match probe_writable_dir(&root) {
            Ok(()) => DoctorCheck::pass(
                "Default data directory",
                format!("{} is writable.", root.display()),
                "No action needed.",
            ),
            Err(e) => DoctorCheck::fail(
                "Default data directory",
                format!("{} is not writable: {e}.", root.display()),
                "Fix permissions or set data_dir in appcipe.yml before packaging the app.",
            ),
        },
        Err(e) => DoctorCheck::fail(
            "Default data directory",
            e.to_string(),
            "Set the required platform environment variable or specify data_dir in appcipe.yml.",
        ),
    }
}

fn default_data_root() -> Result<PathBuf> {
    match std::env::consts::OS {
        "windows" => std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .context("LOCALAPPDATA is not set"),
        "macos" => std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .map(|p| p.join("Library").join("Application Support"))
            .context("HOME is not set"),
        "linux" => {
            if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Ok(PathBuf::from(xdg))
            } else {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .map(|p| p.join(".local").join("share"))
                    .context("neither XDG_DATA_HOME nor HOME is set")
            }
        }
        other => anyhow::bail!("unsupported host OS for default data directory: {other}"),
    }
}

fn probe_writable_dir(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("failed to create {}", root.display()))?;
    let probe = root.join(format!(".chefer-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"ok")
        .with_context(|| format!("failed to write {}", probe.display()))?;
    std::fs::remove_file(&probe)
        .with_context(|| format!("failed to remove {}", probe.display()))?;
    Ok(())
}

fn render_report(report: &DoctorReport) {
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(ui::term_width())
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(20)),
            ColumnConstraint::Absolute(Width::Percentage(10)),
            ColumnConstraint::Absolute(Width::Percentage(35)),
            ColumnConstraint::Absolute(Width::Percentage(35)),
        ]);
    table.set_header(vec![
        Cell::new("Check")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Status")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Detail")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Action")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);
    for check in &report.checks {
        table.add_row(vec![
            Cell::new(&check.name).fg(Color::Cyan),
            Cell::new(check.status.as_str())
                .add_attribute(Attribute::Bold)
                .fg(check.status.color()),
            Cell::new(&check.detail),
            Cell::new(&check.action).fg(Color::Yellow),
        ]);
    }

    let title = if report.has_fail() {
        "▎Doctor found blocking issues".red().bold().to_string()
    } else {
        "▎Doctor diagnostics".green().bold().to_string()
    };
    println!("\n{title}");
    println!("{table}\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_exit_code_tracks_failures() {
        let ok = DoctorReport {
            checks: vec![
                DoctorCheck::pass("a", "ok", "none"),
                DoctorCheck::warn("b", "warn", "fix later"),
            ],
        };
        assert!(!ok.has_fail(), "WARN 不應造成 doctor 失敗");
        assert_eq!(ok.exit_code(), 0);

        let bad = DoctorReport {
            checks: vec![DoctorCheck::fail("a", "bad", "fix")],
        };
        assert!(bad.has_fail(), "任一 FAIL 應造成 doctor 失敗");
        assert_eq!(bad.exit_code(), 1);
    }

    #[test]
    fn linux_userns_classifier_handles_common_settings() {
        let pass = classify_linux_userns(1000, Some("1"), Some("0"));
        assert_eq!(pass.status, DoctorStatus::Pass);

        let disabled = classify_linux_userns(1000, Some("0"), Some("0"));
        assert_eq!(disabled.status, DoctorStatus::Fail);

        let apparmor = classify_linux_userns(1000, Some("1"), Some("1"));
        assert_eq!(apparmor.status, DoctorStatus::Fail);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_backend_checks_include_whp() {
        let checks = check_backend_prerequisites();
        assert!(checks.iter().any(|check| check.name == "WSL2 backend"));

        let whp = checks
            .iter()
            .find(|check| check.name == "WHP backend")
            .expect("Windows diagnostics should include WHP backend check");
        assert!(
            whp.status == DoctorStatus::Pass || whp.status == DoctorStatus::Warn,
            "WHP should be Pass (hypervisor present) or Warn (absent/unavailable)"
        );
        assert!(whp.detail.contains("WHP") || whp.detail.contains("Hypervisor"));
    }

    #[test]
    fn guest_arch_maps_supported_hosts() {
        let arch = host_guest_arch();
        assert!(
            arch == Some("x86_64") || arch == Some("aarch64") || arch.is_none(),
            "host arch mapping should be conservative: {arch:?}"
        );
    }
}
