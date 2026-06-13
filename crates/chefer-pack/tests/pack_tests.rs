//! chefer-pack 整合測試：在測試內合成最小 docker-archive / OCI layout image tar，
//! 不依賴 Docker。驗證 bundle 佈局、manifest 內容、層重壓與 diff_id 計算。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use appcipe_spec::{
    AppCipe, Cmd, ImageFormat, ImagePlatform, ImageSourceOrPath, ImageSourceType, Service,
};
use chefer_bundle::{CmdSpec, Manifest, layout};
use chefer_pack::{PackOptions, pack};
use sha2::{Digest, Sha256};

// ---------- 共用小工具 ----------

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

/// 合成一個最小 layer tar（內含一個檔案），回傳 (tar bytes, diff_id)。
fn make_layer_tar(marker: &str) -> (Vec<u8>, String) {
    let mut b = tar::Builder::new(Vec::new());
    let content = format!("hello from {marker}\n");
    append_file(&mut b, "hello.txt", content.as_bytes());
    let bytes = b.into_inner().unwrap();
    let diff_id = format!("sha256:{}", sha256_hex(&bytes));
    (bytes, diff_id)
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn make_image_config(os: &str, arch: &str, diff_ids: &[&str]) -> Vec<u8> {
    let cfg = serde_json::json!({
        "architecture": arch,
        "os": os,
        "config": {
            "Env": [format!("ARCH={arch}"), "FOO=bar"],
            "Cmd": ["echo", "hi"],
            "Entrypoint": ["/entry"],
            "WorkingDir": "/app",
            "User": "appuser",
            "ExposedPorts": { "80/tcp": {} }
        },
        "rootfs": { "type": "layers", "diff_ids": diff_ids }
    });
    serde_json::to_vec(&cfg).unwrap()
}

/// 合成傳統 docker-archive image tar，回傳 (tar 路徑, 真實 diff_id)。
/// `declared_diff_id` 可覆寫 config 宣告的 diff_id（製造不符案例）。
fn build_docker_archive(
    dir: &Path,
    os: &str,
    arch: &str,
    declared_diff_id: Option<&str>,
) -> (PathBuf, String) {
    let (layer, diff_id) = make_layer_tar("docker-archive");
    let declared = declared_diff_id.unwrap_or(&diff_id);
    let config = make_image_config(os, arch, &[declared]);
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["test:latest"],
        "Layers": ["layer1/layer.tar"]
    }]))
    .unwrap();

    let mut b = tar::Builder::new(Vec::new());
    append_file(&mut b, "config.json", &config);
    append_file(&mut b, "layer1/layer.tar", &layer);
    append_file(&mut b, "manifest.json", &manifest);
    let bytes = b.into_inner().unwrap();

    let p = dir.join("image-docker.tar");
    std::fs::write(&p, bytes).unwrap();
    (p, diff_id)
}

/// 合成 OCI layout image tar（layer 以 gzip 壓縮存放）。
/// `platforms`：每個平台各建一份 manifest；`nested` 時 index.json 內再包一層巢狀 index。
fn build_oci_archive(dir: &Path, platforms: &[(&str, &str)], nested: bool) -> (PathBuf, String) {
    let (layer, diff_id) = make_layer_tar("oci-archive");
    let layer_gz = gzip(&layer);
    let layer_digest_hex = sha256_hex(&layer_gz);

    let mut blobs: Vec<(String, Vec<u8>)> = vec![(layer_digest_hex.clone(), layer_gz.clone())];
    let mut descriptors = Vec::new();

    for (os, arch) in platforms {
        let config = make_image_config(os, arch, &[&diff_id]);
        let config_digest_hex = sha256_hex(&config);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_digest_hex}"),
                "size": config.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_digest_hex}"),
                "size": layer_gz.len()
            }]
        }))
        .unwrap();
        let manifest_digest_hex = sha256_hex(&manifest);
        descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest_hex}"),
            "size": manifest.len(),
            "platform": { "architecture": arch, "os": os }
        }));
        blobs.push((config_digest_hex, config));
        blobs.push((manifest_digest_hex, manifest));
    }

    let index_manifests = if nested {
        // index.json → 巢狀 index blob → 真正的 manifest 清單
        let nested_index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": descriptors
        }))
        .unwrap();
        let nested_digest_hex = sha256_hex(&nested_index);
        let d = serde_json::json!([{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "digest": format!("sha256:{nested_digest_hex}"),
            "size": nested_index.len()
        }]);
        blobs.push((nested_digest_hex, nested_index));
        d
    } else {
        serde_json::Value::Array(descriptors)
    };

    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": index_manifests
    }))
    .unwrap();
    let oci_layout = br#"{"imageLayoutVersion": "1.0.0"}"#;

    let mut b = tar::Builder::new(Vec::new());
    append_file(&mut b, "oci-layout", oci_layout);
    append_file(&mut b, "index.json", &index);
    for (hex, data) in &blobs {
        append_file(&mut b, &format!("blobs/sha256/{hex}"), data);
    }
    let bytes = b.into_inner().unwrap();

    let p = dir.join("image-oci.tar");
    std::fs::write(&p, bytes).unwrap();
    (p, diff_id)
}

fn make_service(image: ImageSourceOrPath) -> Service {
    Service {
        image,
        cmd: None,
        workdir: None,
        env: HashMap::new(),
        persist_path: None,
        ports: vec![],
        mounts: vec![],
        interface_mode: Default::default(),
        depends_on: vec![],
    }
}

fn make_app(name: &str, services: Vec<(&str, Service)>) -> AppCipe {
    AppCipe {
        version: "0.1".to_string(),
        name: name.to_string(),
        app_version: Some("1.0.0".to_string()),
        old_names: vec![],
        data_dir: None,
        crash: Default::default(),
        services: services
            .into_iter()
            .map(|(n, s)| (n.to_string(), s))
            .collect(),
    }
}

fn default_opts(out: &Path) -> PackOptions {
    PackOptions {
        out_dir: out.to_path_buf(),
        clean: true,
        write_original_yml: false,
        kit_dirs: vec![],
        target_triples: vec![],
        require_agents: false,
        zstd_level: 3,
    }
}

/// 驗證 bundle 內的層檔可由 zstd 解回，且內容 sha256 == diff_id。
fn assert_layer_roundtrip(bundle_dir: &Path, rel_path: &str, expect_diff_id: &str) {
    let p = bundle_dir.join(rel_path);
    assert!(p.is_file(), "層檔不存在：{}", p.display());
    let decoded = zstd::stream::decode_all(std::fs::File::open(&p).unwrap()).unwrap();
    assert_eq!(
        format!("sha256:{}", sha256_hex(&decoded)),
        expect_diff_id,
        "層內容解回後 sha256 與 diff_id 不符"
    );
}

// ---------- 測試 ----------

#[test]
fn pack_docker_archive_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, diff_id) = build_docker_archive(tmp.path(), "linux", "amd64", None);

    let mut svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    svc.ports = vec!["8080:80".to_string()];
    svc.env.insert("FOO".to_string(), "override".to_string());
    svc.cmd = Some(Cmd::String("echo override".to_string()));
    svc.persist_path = Some("/var/data".to_string());

    let app = make_app("TestApp", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let res = pack(&app, &default_opts(&out)).unwrap();

    // bundle 佈局
    assert_eq!(res.bundle_dir, out.join("TestApp").join("bundle"));
    assert!(layout::manifest_path(&res.bundle_dir).is_file());

    // 由檔案重新載入 manifest，確認與回傳值一致
    let m = Manifest::load(&layout::manifest_path(&res.bundle_dir)).unwrap();
    assert_eq!(m.app.name, "TestApp");
    assert_eq!(m.app.spec_version, "0.1");
    assert_eq!(m.app.app_version.as_deref(), Some("1.0.0"));
    assert!(!m.app.generated_at_utc.is_empty());
    assert_eq!(m.services.len(), 1);

    let s = &m.services[0];
    assert_eq!(s.name, "web");
    assert_eq!(s.platform, "linux/amd64");
    assert_eq!(s.layers.len(), 1);
    assert_eq!(s.layers[0].diff_id, diff_id);
    let expected_rel = format!(
        "services/web/layers/{}",
        layout::layer_file_name(0, &diff_id)
    );
    assert_eq!(s.layers[0].rel_path, expected_rel);
    assert!(s.layers[0].size.unwrap() > 0);

    // image config 抽取
    assert_eq!(s.image_config.entrypoint, vec!["/entry"]);
    assert_eq!(s.image_config.cmd, vec!["echo", "hi"]);
    assert!(s.image_config.env.contains(&"FOO=bar".to_string()));
    assert_eq!(s.image_config.working_dir.as_deref(), Some("/app"));
    assert_eq!(s.image_config.user.as_deref(), Some("appuser"));
    assert_eq!(s.image_config.exposed_ports, vec!["80/tcp"]);

    // appcipe 覆蓋
    assert!(matches!(&s.cmd_override, Some(CmdSpec::Shell(c)) if c == "echo override"));
    assert_eq!(s.env.get("FOO").map(String::as_str), Some("override"));
    assert_eq!(s.persist_path.as_deref(), Some("/var/data"));
    assert_eq!(s.ports.len(), 1);
    assert_eq!(s.ports[0].host, 8080);
    assert_eq!(s.ports[0].guest, 80);

    // 層檔可解回且 diff_id 正確
    assert_layer_roundtrip(&res.bundle_dir, &s.layers[0].rel_path, &diff_id);
}

#[test]
fn pack_oci_archive_gzip_layer() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, diff_id) = build_oci_archive(tmp.path(), &[("linux", "amd64")], false);

    let svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    let app = make_app("OciApp", vec![("api", svc)]);
    let out = tmp.path().join("out");
    let res = pack(&app, &default_opts(&out)).unwrap();

    let s = &res.manifest.services[0];
    assert_eq!(s.name, "api");
    assert_eq!(s.layers.len(), 1);
    // gzip → zstd 轉換後，diff_id（未壓縮 tar 的 sha256）必須正確
    assert_eq!(s.layers[0].diff_id, diff_id);
    assert_layer_roundtrip(&res.bundle_dir, &s.layers[0].rel_path, &diff_id);
    assert_eq!(s.image_config.cmd, vec!["echo", "hi"]);
}

#[test]
fn pack_oci_nested_index_and_platform_selection() {
    let tmp = tempfile::tempdir().unwrap();
    // 巢狀 index + 兩個平台；service 要求 arm64
    let (image_tar, diff_id) =
        build_oci_archive(tmp.path(), &[("linux", "amd64"), ("linux", "arm64")], true);

    let svc = make_service(ImageSourceOrPath::Full {
        source: ImageSourceType::Tar,
        file: image_tar.to_string_lossy().to_string(),
        format: ImageFormat::Auto,
        platform: ImagePlatform::LinuxArm64,
    });
    let app = make_app("MultiArch", vec![("db", svc)]);
    let out = tmp.path().join("out");
    let res = pack(&app, &default_opts(&out)).unwrap();

    let s = &res.manifest.services[0];
    assert_eq!(s.platform, "linux/arm64");
    // 挑到 arm64 的 config（Env 內嵌了 arch 標記）
    assert!(s.image_config.env.contains(&"ARCH=arm64".to_string()));
    assert_eq!(s.layers[0].diff_id, diff_id);
    assert_layer_roundtrip(&res.bundle_dir, &s.layers[0].rel_path, &diff_id);
}

#[test]
fn pack_rejects_platform_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    // archive 只有 linux/arm64，但 service（TarPath 短寫）預設要求 linux/amd64
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "arm64", None);

    let svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    let app = make_app("Mismatch", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let err = pack(&app, &default_opts(&out)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("找不到平台 linux/amd64"), "{msg}");
    assert!(msg.contains("linux/arm64"), "應列出可用平台：{msg}");
}

#[test]
fn pack_rejects_diff_id_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let bogus = format!("sha256:{}", "0".repeat(64));
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "amd64", Some(&bogus));

    let svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    let app = make_app("BadDiff", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let err = pack(&app, &default_opts(&out)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("diff_id 不符"), "{msg}");
}

#[test]
fn pack_rejects_invalid_archive() {
    let tmp = tempfile::tempdir().unwrap();
    // 一個既非 OCI 也非 docker-archive 的 tar
    let mut b = tar::Builder::new(Vec::new());
    append_file(&mut b, "random.txt", b"not an image");
    let bytes = b.into_inner().unwrap();
    let p = tmp.path().join("not-image.tar");
    std::fs::write(&p, bytes).unwrap();

    let svc = make_service(ImageSourceOrPath::TarPath(p.to_string_lossy().to_string()));
    let app = make_app("NotImage", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let err = pack(&app, &default_opts(&out)).unwrap_err();
    assert!(format!("{err:#}").contains("不是有效的 image archive"));
}

#[test]
fn pack_rejects_missing_mount_host_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "amd64", None);

    let missing = tmp.path().join("definitely-missing-dir");
    let mut svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    svc.mounts = vec![format!("{}:/data", missing.to_string_lossy())];

    let app = make_app("BadMount", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let err = pack(&app, &default_opts(&out)).unwrap_err();
    assert!(format!("{err:#}").contains("掛載 host 路徑不存在"));
}

#[test]
fn pack_sorts_services_and_writes_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "amd64", None);
    let path_str = image_tar.to_string_lossy().to_string();

    let app = make_app(
        "SortedApp",
        vec![
            (
                "zeta",
                make_service(ImageSourceOrPath::TarPath(path_str.clone())),
            ),
            ("alpha", make_service(ImageSourceOrPath::TarPath(path_str))),
        ],
    );
    let out = tmp.path().join("out");
    let mut opts = default_opts(&out);
    opts.write_original_yml = true;
    let res = pack(&app, &opts).unwrap();

    // services 依 name 排序
    let names: Vec<&str> = res
        .manifest
        .services
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, vec!["alpha", "zeta"]);

    // appcipe.yml 回寫且可被 YAML 解析
    let yml_path = layout::appcipe_out_path(&res.bundle_dir);
    assert!(yml_path.is_file());
    let yml = std::fs::read_to_string(&yml_path).unwrap();
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yml).unwrap();
    assert_eq!(
        parsed.get("name").and_then(|v| v.as_str()),
        Some("SortedApp")
    );
}

#[test]
fn pack_copies_guest_agent_from_kit() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "amd64", None);

    // 準備假的 kit 目錄
    let kit_dir = tmp.path().join("kit");
    std::fs::create_dir_all(&kit_dir).unwrap();
    std::fs::write(kit_dir.join("guest-agent-x86_64"), b"fake-musl-agent").unwrap();

    let svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    let app = make_app("AgentApp", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let mut opts = default_opts(&out);
    opts.kit_dirs = vec![kit_dir];
    opts.require_agents = true;
    let res = pack(&app, &opts).unwrap();

    let agent = layout::agents_dir(&res.bundle_dir).join(layout::guest_agent_name("x86_64"));
    assert!(agent.is_file(), "guest-agent 未複製到 agents/");
    assert_eq!(std::fs::read(&agent).unwrap(), b"fake-musl-agent");
}

#[test]
fn pack_copies_macos_appliance_from_kit_when_targeting_darwin() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "arm64", None);

    let kit_dir = tmp.path().join("kit");
    std::fs::create_dir_all(&kit_dir).unwrap();
    std::fs::write(kit_dir.join("guest-agent-aarch64"), b"fake-musl-agent").unwrap();
    std::fs::write(kit_dir.join("chefer-vmlinuz-aarch64"), b"fake-kernel").unwrap();
    std::fs::write(kit_dir.join("chefer-initramfs-aarch64"), b"fake-initramfs").unwrap();

    let svc = make_service(ImageSourceOrPath::Full {
        source: ImageSourceType::Tar,
        file: image_tar.to_string_lossy().to_string(),
        format: ImageFormat::DockerArchive,
        platform: ImagePlatform::LinuxArm64,
    });
    let app = make_app("MacApplianceApp", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let mut opts = default_opts(&out);
    opts.kit_dirs = vec![kit_dir];
    opts.target_triples = vec!["aarch64-apple-darwin".to_string()];
    opts.require_agents = true;
    let res = pack(&app, &opts).unwrap();

    let vm = layout::vm_dir(&res.bundle_dir);
    assert_eq!(
        std::fs::read(vm.join(layout::kernel_name("aarch64"))).unwrap(),
        b"fake-kernel"
    );
    assert_eq!(
        std::fs::read(vm.join(layout::initramfs_name("aarch64"))).unwrap(),
        b"fake-initramfs"
    );
}

#[test]
fn pack_skips_missing_macos_appliance_without_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "arm64", None);

    let kit_dir = tmp.path().join("kit");
    std::fs::create_dir_all(&kit_dir).unwrap();
    std::fs::write(kit_dir.join("guest-agent-aarch64"), b"fake-musl-agent").unwrap();

    let svc = make_service(ImageSourceOrPath::Full {
        source: ImageSourceType::Tar,
        file: image_tar.to_string_lossy().to_string(),
        format: ImageFormat::DockerArchive,
        platform: ImagePlatform::LinuxArm64,
    });
    let app = make_app("MissingMacApplianceApp", vec![("web", svc)]);
    let out = tmp.path().join("out");
    let mut opts = default_opts(&out);
    opts.kit_dirs = vec![kit_dir];
    opts.target_triples = vec!["aarch64-apple-darwin".to_string()];
    opts.require_agents = true;

    let res = pack(&app, &opts).unwrap();
    assert!(
        !layout::vm_dir(&res.bundle_dir).exists(),
        "缺少 appliance 時應略過 vm/，不阻斷建置"
    );
}

#[test]
fn pack_refuses_dirty_output_without_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let (image_tar, _) = build_docker_archive(tmp.path(), "linux", "amd64", None);
    let svc = make_service(ImageSourceOrPath::TarPath(
        image_tar.to_string_lossy().to_string(),
    ));
    let app = make_app("DirtyOut", vec![("web", svc)]);
    let out = tmp.path().join("out");

    // 先放一個殘留檔在 bundle 目錄
    let bundle_dir = out.join("DirtyOut").join("bundle");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(bundle_dir.join("stale.bin"), b"stale").unwrap();

    let mut opts = default_opts(&out);
    opts.clean = false;
    let err = pack(&app, &opts).unwrap_err();
    assert!(format!("{err:#}").contains("已存在且非空"));

    // clean=true 則成功且殘留檔被清掉
    opts.clean = true;
    let res = pack(&app, &opts).unwrap();
    assert!(!res.bundle_dir.join("stale.bin").exists());
}
