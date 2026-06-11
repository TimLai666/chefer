//! chefer-cli 端到端整合測試：
//! 在測試內合成最小 docker-archive image tar（不依賴 Docker）與假 runtime kit，
//! 透過實際編出的 chefer-cli 執行檔跑 init / check / build / inspect 全流程。

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// 測試用 target（linux 目標：不要求內嵌 guest-agent）。
const TEST_TARGET: &str = "x86_64-unknown-linux-gnu";

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chefer-cli"))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn append_file(b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Regular);
    b.append_data(&mut h, path, data).unwrap();
}

/// 合成最小 docker-archive image tar（單層、linux/amd64）。
fn build_docker_archive(dir: &Path) -> PathBuf {
    // layer tar：內含一個檔案
    let mut lb = tar::Builder::new(Vec::new());
    append_file(&mut lb, "hello.txt", b"hello from cli e2e\n");
    let layer = lb.into_inner().unwrap();
    let diff_id = format!("sha256:{}", sha256_hex(&layer));

    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Env": ["FOO=bar"],
            "Cmd": ["echo", "hi"],
            "Entrypoint": ["/entry"]
        },
        "rootfs": { "type": "layers", "diff_ids": [diff_id] }
    }))
    .unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["e2e:latest"],
        "Layers": ["layer1/layer.tar"]
    }]))
    .unwrap();

    let mut b = tar::Builder::new(Vec::new());
    append_file(&mut b, "config.json", &config);
    append_file(&mut b, "layer1/layer.tar", &layer);
    append_file(&mut b, "manifest.json", &manifest);
    let bytes = b.into_inner().unwrap();

    let p = dir.join("app-image.tar");
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn build_then_inspect_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let image_tar = build_docker_archive(tmp.path());

    // appcipe.yml（image 路徑相對於設定檔所在目錄）
    let yml = tmp.path().join("appcipe.yml");
    std::fs::write(
        &yml,
        format!(
            "version: \"0.1\"\nname: E2eApp\napp_version: \"1.0.0\"\nservices:\n  web:\n    image: {}\n    ports: [\"8080:80\"]\n",
            image_tar.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    // 假 runtime kit（assemble 只要求檔案存在，不驗證其為真執行檔）
    let kit = tmp.path().join("kit");
    std::fs::create_dir_all(&kit).unwrap();
    std::fs::write(
        kit.join(format!("chefer-runtime-{TEST_TARGET}")),
        b"fake runtime bytes for e2e",
    )
    .unwrap();

    let out_dir = tmp.path().join("dist");

    // ── build ───────────────────────────────────────────────────
    let output = cli()
        .arg("build")
        .arg(&yml)
        .arg("--out")
        .arg(&out_dir)
        .arg("--target")
        .arg(TEST_TARGET)
        .arg("--kit-dir")
        .arg(&kit)
        .output()
        .expect("無法執行 chefer-cli");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "build 失敗：\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // 輸出檔名規則：<out>/<name>/<name>_<target>（linux 目標無 .exe）
    let artifact = out_dir.join("E2eApp").join(format!("E2eApp_{TEST_TARGET}"));
    assert!(artifact.is_file(), "找不到產物：{}", artifact.display());

    // footer 可讀且 payload 落在檔案範圍內
    let footer = chefer_bundle::Footer::read_from_file(&artifact).unwrap();
    assert!(footer.is_zstd());
    assert_eq!(footer.offset, b"fake runtime bytes for e2e".len() as u64);

    // ── inspect ────────────────────────────────────────────────
    let output = cli()
        .arg("inspect")
        .arg(&artifact)
        .output()
        .expect("無法執行 chefer-cli");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "inspect 失敗：\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("E2eApp"),
        "inspect 輸出應含 app 名稱：{stdout}"
    );
    assert!(
        stdout.contains("web"),
        "inspect 輸出應含 service 名稱：{stdout}"
    );
}

#[test]
fn build_fails_with_actionable_error_when_runtime_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let image_tar = build_docker_archive(tmp.path());

    let yml = tmp.path().join("appcipe.yml");
    std::fs::write(
        &yml,
        format!(
            "version: \"0.1\"\nname: NoKitApp\nservices:\n  web:\n    image: {}\n",
            image_tar.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    // 空 kit 目錄 + 非 host 的 target → 找不到 runtime
    let kit = tmp.path().join("empty-kit");
    std::fs::create_dir_all(&kit).unwrap();

    let output = cli()
        .arg("build")
        .arg(&yml)
        .arg("--out")
        .arg(tmp.path().join("dist"))
        .arg("--target")
        .arg("riscv64gc-unknown-linux-gnu")
        .arg("--kit-dir")
        .arg(&kit)
        .output()
        .expect("無法執行 chefer-cli");
    assert!(!output.status.success(), "缺 runtime 時 build 應失敗");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("chefer-runtime-riscv64gc-unknown-linux-gnu"),
        "錯誤訊息應含期望檔名：{stderr}"
    );
    assert!(
        stderr.contains("kit"),
        "錯誤訊息應提示 kit 取得方式：{stderr}"
    );
}

#[test]
fn init_then_check_succeeds() {
    let tmp = tempfile::tempdir().unwrap();

    let output = cli()
        .arg("init")
        .arg(tmp.path())
        .output()
        .expect("無法執行 chefer-cli");
    assert!(output.status.success());
    assert!(tmp.path().join("appcipe.yml").is_file());

    let output = cli()
        .arg("check")
        .arg(tmp.path())
        .output()
        .expect("無法執行 chefer-cli");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check 失敗：\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("MyApp"), "{stdout}");
}
