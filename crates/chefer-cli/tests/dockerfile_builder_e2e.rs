//! 真 CLI + 隔離 PATH 的 Windows builder 相容性測試，不需要 Docker 或 WSL。
#![cfg(windows)]

use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const TARGET: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-gnu"
} else {
    "x86_64-unknown-linux-gnu"
};
const IMAGE_ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "amd64"
};
const PLATFORM: &str = if cfg!(target_arch = "aarch64") {
    "linux/arm64"
} else {
    "linux/amd64"
};
const OTHER_PLATFORM: &str = if cfg!(target_arch = "aarch64") {
    "linux/amd64"
} else {
    "linux/arm64"
};

fn fake_builder() -> &'static Path {
    static COMPILED: OnceLock<tempfile::TempDir> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("builder.rs");
        std::fs::write(&src, r#"
use std::{env, fs::{self, OpenOptions}, io::Write, process};
fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let tool = env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().into_owned();
    let mut record = vec![tool]; record.extend(args.clone());
    let mut log = OpenOptions::new().create(true).append(true).open(env::var("CHEFER_TEST_LOG").unwrap()).unwrap();
    log.write_all(&(record.len() as u32).to_le_bytes()).unwrap();
    for arg in record { log.write_all(&(arg.len() as u32).to_le_bytes()).unwrap(); log.write_all(arg.as_bytes()).unwrap(); }
    if args == ["--version"] { return; }
    if args.first().map(String::as_str) == Some("image") { args.remove(0); }
    let op = args.first().map(String::as_str).unwrap_or("");
    let mode = env::var("CHEFER_TEST_FAILURE").unwrap_or_default();
    if op == mode || (op == "rm" && mode == "cleanup") { eprintln!("FAKE_{op}_FAILURE"); process::exit(23); }
    match op {
        "build" | "rm" => (),
        "save" => { let n = args.iter().position(|a| a == "-o").unwrap(); fs::copy(env::var("CHEFER_TEST_IMAGE_TAR").unwrap(), &args[n+1]).unwrap(); },
        _ => process::exit(24),
    }
}
"#).unwrap();
        let output = Command::new("rustc").arg("--edition=2024").arg(&src)
            .arg("-o").arg(dir.path().join("builder.exe")).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        dir
    }).path()
}

fn append(tar: &mut tar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_mode(0o644);
    h.set_size(bytes.len() as u64);
    tar.append_data(&mut h, name, bytes).unwrap();
}

struct Fixture {
    dir: tempfile::TempDir,
    tools: PathBuf,
    log: PathBuf,
}
impl Fixture {
    fn new(builders: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        for tool in builders {
            std::fs::copy(
                fake_builder().join("builder.exe"),
                tools.join(format!("{tool}.exe")),
            )
            .unwrap();
        }
        let ctx = dir.path().join("context with spaces");
        std::fs::create_dir(&ctx).unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(dir.path().join("appcipe.yml"), "version: \"0.1\"\nname: BuilderE2e\nservices:\n  app:\n    image:\n      source: dockerfile\n      file: './context with spaces/Dockerfile'\n      context: './context with spaces'\n      build_args:\n        Z: 'last value'\n        A: 'first value'\n").unwrap();
        let recipe = dir.path().join("appcipe.yml");
        let text = std::fs::read_to_string(&recipe).unwrap().replace(
            "source: dockerfile",
            &format!("source: dockerfile\n      platform: {PLATFORM}"),
        );
        std::fs::write(&recipe, text).unwrap();
        let mut layer = tar::Builder::new(Vec::new());
        append(&mut layer, "hello", b"builder fixture");
        let layer = layer.into_inner().unwrap();
        let digest = Sha256::digest(&layer)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let config = serde_json::to_vec(&serde_json::json!({"os":"linux", "architecture":IMAGE_ARCH, "config":{"Cmd":["true"]}, "rootfs":{"type":"layers", "diff_ids":[format!("sha256:{digest}")]}})).unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        append(&mut archive, "layer.tar", &layer);
        append(&mut archive, "config.json", &config);
        append(
            &mut archive,
            "manifest.json",
            br#"[{"Config":"config.json","RepoTags":["fixture:1"],"Layers":["layer.tar"]}]"#,
        );
        std::fs::write(
            dir.path().join("fixture.tar"),
            archive.into_inner().unwrap(),
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("kit")).unwrap();
        std::fs::write(
            dir.path()
                .join("kit")
                .join(format!("chefer-runtime-{TARGET}")),
            b"fake runtime",
        )
        .unwrap();
        let log = dir.path().join("calls.bin");
        Self { dir, tools, log }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chefer-cli"));
        cmd.current_dir(self.dir.path())
            .env("PATH", &self.tools)
            .env_remove("CHEFER_DOCKERFILE_BUILDER")
            .env("CHEFER_TEST_LOG", &self.log)
            .env("CHEFER_TEST_IMAGE_TAR", self.dir.path().join("fixture.tar"));
        cmd
    }
    fn build(&self, forced: Option<&str>, failure: &str) -> Output {
        let mut cmd = self.command();
        if let Some(tool) = forced {
            cmd.env("CHEFER_DOCKERFILE_BUILDER", tool);
        }
        cmd.env("CHEFER_TEST_FAILURE", failure)
            .args([
                "build",
                "appcipe.yml",
                "--out",
                "dist",
                "--kit-dir",
                "kit",
                "--target",
                TARGET,
            ])
            .output()
            .unwrap()
    }
    fn calls(&self) -> Vec<Vec<String>> {
        let mut input = Cursor::new(std::fs::read(&self.log).unwrap_or_default());
        let mut calls = Vec::new();
        loop {
            let mut count = [0; 4];
            if input.read_exact(&mut count).is_err() {
                break;
            }
            let mut args = Vec::new();
            for _ in 0..u32::from_le_bytes(count) {
                let mut len = [0; 4];
                input.read_exact(&mut len).unwrap();
                let mut bytes = vec![0; u32::from_le_bytes(len) as usize];
                input.read_exact(&mut bytes).unwrap();
                args.push(String::from_utf8(bytes).unwrap());
            }
            calls.push(args);
        }
        calls
    }
}

#[test]
fn detects_wslc_and_packs_its_export_with_preserved_arguments() {
    let f = Fixture::new(&["wslc"]);
    let out = f.build(None, "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = f.calls();
    assert_eq!(&calls[0], &["wslc", "--version"]);
    let build = &calls[1];
    assert_eq!(
        &build[..7],
        &[
            "wslc",
            "build",
            "--build-arg",
            "A=first value",
            "--build-arg",
            "Z=last value",
            "-f"
        ]
    );
    assert!(
        Path::new(&build[7]).ends_with(Path::new("context with spaces").join("Dockerfile")),
        "{build:?}"
    );
    assert!(build.last().unwrap().ends_with("context with spaces"));
    assert_eq!(&calls[2][..4], &["wslc", "image", "save", "-o"]);
    assert_eq!(&calls[3][..4], &["wslc", "image", "rm", "-f"]);
    let artifact = f
        .dir
        .path()
        .join("dist/BuilderE2e")
        .join(format!("BuilderE2e_{TARGET}"));
    assert!(
        chefer_bundle::Footer::read_from_file(&artifact)
            .unwrap()
            .is_zstd()
    );
}

#[test]
fn explicitly_selected_wslc_bypasses_docker_and_doctor_reports_it() {
    let f = Fixture::new(&["docker", "wslc"]);
    let out = f.build(Some("wslc"), "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.calls().iter().all(|args| args[0] == "wslc"));
    let out = f
        .command()
        .env("CHEFER_DOCKERFILE_BUILDER", "wslc")
        .args(["doctor"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Dockerfile builder") && text.contains("Found `wslc`"),
        "{text}"
    );
}

#[test]
fn existing_docker_priority_is_preserved() {
    let f = Fixture::new(&["docker", "wslc"]);
    let out = f.build(None, "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.calls().iter().all(|args| args[0] == "docker"));
    assert_eq!(&f.calls()[2][..3], &["docker", "save", "-o"]);
}

#[test]
fn unavailable_forced_wslc_does_not_fall_back_to_docker() {
    let f = Fixture::new(&["docker"]);
    let out = f.build(Some("wslc"), "");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not runnable on PATH"));
    assert!(f.calls().is_empty());
}

#[test]
fn failed_build_does_not_export_and_failed_export_is_cleaned_up() {
    for failure in ["build", "save"] {
        let f = Fixture::new(&["wslc"]);
        let out = f.build(Some("wslc"), failure);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("FAKE_{failure}_FAILURE")),
            "{stderr}"
        );
        let calls = f.calls();
        if failure == "build" {
            assert_eq!(calls.len(), 2);
        } else {
            assert_eq!(&calls.last().unwrap()[..4], &["wslc", "image", "rm", "-f"]);
        }
    }
}

#[test]
fn cleanup_failure_does_not_discard_successful_export() {
    let f = Fixture::new(&["wslc"]);
    let out = f.build(Some("wslc"), "cleanup");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn wslc_rejects_cross_arch_before_building() {
    let f = Fixture::new(&["wslc"]);
    let yml = f.dir.path().join("appcipe.yml");
    let text = std::fs::read_to_string(&yml)
        .unwrap()
        .replace(PLATFORM, OTHER_PLATFORM);
    std::fs::write(&yml, text).unwrap();
    let out = f.build(Some("wslc"), "");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(OTHER_PLATFORM) && err.contains("native") && err.contains("docker"),
        "{err}"
    );
    assert_eq!(
        f.calls(),
        vec![vec!["wslc".to_string(), "--version".to_string()]]
    );
}

#[test]
fn windows_container_alias_uses_wslc_commands() {
    let f = Fixture::new(&["container"]);
    let out = f.build(Some("container"), "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = f.calls();
    assert_eq!(&calls[1][..3], &["container", "build", "--build-arg"]);
    assert_eq!(&calls[2][..4], &["container", "image", "save", "-o"]);
}
