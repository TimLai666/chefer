//! `chefer inspect <file>` — 檢視 Chefer 單一執行檔的 footer 與內嵌 manifest 摘要。
//!
//! 行為（DESIGN.md §6 chefer-cli 節）：
//! 1. `chefer_bundle::Footer::read_from_file` → 列印 footer 資訊表格。
//! 2. 串流讀 payload（zstd 解壓 + tar 逐 entry），**只取** `bundle/manifest.json`，
//!    不執行、不解壓其他內容（層檔可能數 GB）。

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use comfy_table::{Cell, Color};
use owo_colors::OwoColorize;

use crate::ui;

/// manifest.json 的大小上限（防禦損毀檔案宣告超大 entry 導致 OOM）。
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// 網路模式的人類可讀標籤（含對外可達性的一句話說明）。
fn network_label(n: chefer_bundle::NetworkMode) -> &'static str {
    use chefer_bundle::NetworkMode::*;
    match n {
        Shared => "shared (shares host network; even undeclared ports reachable)",
        Internal => "internal (isolated; only declared ports, no outbound)",
        Bridge => "bridge (isolated; only declared ports, outbound allowed)",
    }
}

/// 共用主控台顯示策略的人類可讀標籤。
fn console_label(c: chefer_bundle::ConsoleMode) -> &'static str {
    use chefer_bundle::ConsoleMode::*;
    match c {
        Auto => "auto (hidden for gui-only; shown with a terminal or no interface)",
        Shown => "shown (always show shared console)",
        Hidden => "hidden (hide shared console)",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompanionInventory {
    agents: Vec<String>,
    vm: Vec<String>,
}

#[derive(Debug, Clone)]
struct InspectPayload {
    manifest: chefer_bundle::Manifest,
    companions: CompanionInventory,
}

pub fn cmd_inspect(file: &Path) -> Result<()> {
    let footer = chefer_bundle::Footer::read_from_file(file)?;
    let file_size = std::fs::metadata(file)
        .with_context(|| format!("failed to read file metadata: {}", file.display()))?
        .len();

    println!(
        "{}  {}",
        "✔ Chefer single-file".green().bold(),
        file.display().to_string().blue()
    );

    // ── Footer 資訊表格 ─────────────────────────────────────────
    let mut t = ui::kv_table(32, 68);
    t.add_row(vec![
        Cell::new("Footer Version").fg(Color::Cyan),
        Cell::new(footer.version),
    ]);
    t.add_row(vec![
        Cell::new("Payload Compression").fg(Color::Cyan),
        Cell::new(if footer.is_zstd() { "zstd" } else { "none" }).fg(Color::Yellow),
    ]);
    t.add_row(vec![
        Cell::new("Runtime Size (offset)").fg(Color::Cyan),
        Cell::new(format!(
            "{} bytes ({})",
            footer.offset,
            ui::human_bytes(footer.offset)
        )),
    ]);
    t.add_row(vec![
        Cell::new("Payload Size").fg(Color::Cyan),
        Cell::new(format!(
            "{} bytes ({})",
            footer.length,
            ui::human_bytes(footer.length)
        )),
    ]);
    t.add_row(vec![
        Cell::new("Payload SHA-256").fg(Color::Cyan),
        Cell::new(ui::hex(&footer.sha256)),
    ]);
    t.add_row(vec![
        Cell::new("File Size").fg(Color::Cyan),
        Cell::new(format!(
            "{} bytes ({})",
            file_size,
            ui::human_bytes(file_size)
        )),
    ]);

    println!();
    println!("{}", "▎Footer".bold());
    println!("{t}");

    // ── 串流取出 manifest.json 並列印摘要 ───────────────────────
    let payload = read_embedded_payload(file, &footer)?;
    render_manifest_summary(&payload.manifest, footer.length, &payload.companions);
    Ok(())
}

/// 串流讀取 payload，取出 `bundle/manifest.json` 並掃描內嵌協力檔清單。
/// 不把 bundle 解到磁碟，也不把 layer 內容讀進記憶體。
fn read_embedded_payload(path: &Path, footer: &chefer_bundle::Footer) -> Result<InspectPayload> {
    if !footer.is_zstd() {
        bail!(
            "unsupported payload format (flags={:#04x}): this version only supports zstd-compressed tar payloads",
            footer.flags
        );
    }

    let mut f = std::fs::File::open(path)
        .with_context(|| format!("failed to open file: {}", path.display()))?;
    f.seek(SeekFrom::Start(footer.offset))
        .context("failed to seek to payload start")?;
    let limited = BufReader::new(f).take(footer.length);
    let decoder = zstd::stream::read::Decoder::new(limited)
        .context("failed to create zstd decoder (payload may be corrupted)")?;
    // 在 tar 之前限制解壓「輸出」總量：擋下長檔名/pax 標頭撐爆 read_to_end 的炸彈。
    // inspect 需要掃描 agents/vm entry，因此用與壓縮 payload 大小成比例的既有安全邊界。
    let guarded =
        chefer_bundle::LimitedReader::new(decoder, chefer_bundle::bomb_limit_for(footer.length));
    let mut archive = tar::Archive::new(guarded);
    let mut manifest = None;
    let mut companions = CompanionInventory::default();

    for entry in archive.entries().context("failed to read payload tar")? {
        let mut entry = entry.context("failed to read payload tar entry")?;
        let entry_path = entry
            .path()
            .context("failed to read tar entry path")?
            .to_string_lossy()
            .replace('\\', "/");
        if entry.header().entry_type().is_file() {
            record_companion(&mut companions, &entry_path);
        }
        if entry_path != "bundle/manifest.json" {
            continue; // 其他 entry 一律跳過（iterator 會自行略過內容，不落地）
        }
        if entry.size() > MAX_MANIFEST_BYTES {
            bail!(
                "manifest.json too large ({} bytes, limit {} bytes); file may be corrupted",
                entry.size(),
                MAX_MANIFEST_BYTES
            );
        }
        let mut s = String::new();
        entry
            .read_to_string(&mut s)
            .context("failed to read manifest.json contents")?;
        let m: chefer_bundle::Manifest =
            serde_json::from_str(&s).context("failed to parse embedded manifest.json")?;
        if m.format_version != chefer_bundle::MANIFEST_FORMAT_VERSION {
            bail!(
                "unsupported manifest format version {} (this version supports {}); repack with a matching chefer version",
                m.format_version,
                chefer_bundle::MANIFEST_FORMAT_VERSION
            );
        }
        manifest = Some(m);
    }
    let Some(manifest) = manifest else {
        bail!(
            "bundle/manifest.json not found in payload; file may not be a Chefer single-file or is corrupted"
        );
    };
    companions.agents.sort();
    companions.vm.sort();
    Ok(InspectPayload {
        manifest,
        companions,
    })
}

fn record_companion(companions: &mut CompanionInventory, entry_path: &str) {
    if let Some(name) = entry_path.strip_prefix("bundle/agents/") {
        if !name.is_empty() && !name.contains('/') {
            companions.agents.push(name.to_string());
        }
    } else if let Some(name) = entry_path.strip_prefix("bundle/vm/")
        && !name.is_empty()
        && !name.contains('/')
    {
        companions.vm.push(name.to_string());
    }
}

/// 列印 manifest 的 app 與 services 摘要表格。
fn render_manifest_summary(
    m: &chefer_bundle::Manifest,
    payload_len: u64,
    companions: &CompanionInventory,
) {
    use comfy_table::{
        Attribute, ColumnConstraint, ContentArrangement, Table, Width, presets::UTF8_BORDERS_ONLY,
    };

    // ── App ─────────────────────────────────────────────────────
    let mut t = ui::kv_table(32, 68);
    t.add_row(vec![
        Cell::new("Name").fg(Color::Cyan),
        Cell::new(&m.app.name),
    ]);
    if let Some(v) = &m.app.app_version {
        t.add_row(vec![Cell::new("App Version").fg(Color::Cyan), Cell::new(v)]);
    }
    t.add_row(vec![
        Cell::new("Spec Version").fg(Color::Cyan),
        Cell::new(&m.app.spec_version),
    ]);
    t.add_row(vec![
        Cell::new("Crash Policy").fg(Color::Cyan),
        Cell::new(format!("{:?}", m.app.crash)).fg(Color::Yellow),
    ]);
    t.add_row(vec![
        Cell::new("Network").fg(Color::Cyan),
        Cell::new(network_label(m.app.network)).fg(Color::Yellow),
    ]);
    t.add_row(vec![
        Cell::new("Console").fg(Color::Cyan),
        Cell::new(console_label(m.app.console)).fg(Color::Yellow),
    ]);
    t.add_row(vec![
        Cell::new("Payload Size").fg(Color::Cyan),
        Cell::new(format!(
            "{} bytes ({})",
            payload_len,
            ui::human_bytes(payload_len)
        )),
    ]);
    if let Some(d) = &m.app.data_dir_override {
        t.add_row(vec![
            Cell::new("Data Dir Override").fg(Color::Cyan),
            Cell::new(d),
        ]);
    }
    if !m.app.old_names.is_empty() {
        t.add_row(vec![
            Cell::new("Old Names").fg(Color::Cyan),
            Cell::new(m.app.old_names.join(", ")).fg(Color::Magenta),
        ]);
    }
    t.add_row(vec![
        Cell::new("Generated At (UTC)").fg(Color::Cyan),
        Cell::new(&m.app.generated_at_utc),
    ]);
    t.add_row(vec![
        Cell::new("Packed by chefer").fg(Color::Cyan),
        Cell::new(if m.app.builder_version.is_empty() {
            "(unknown / pre-0.x bundle)"
        } else {
            m.app.builder_version.as_str()
        }),
    ]);

    println!("{}", "▎App (manifest)".bold());
    println!("{t}");
    println!();

    // ── Services ───────────────────────────────────────────────
    let mut t = Table::new();
    t.load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(ui::term_width())
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(10)), // Service
            ColumnConstraint::Absolute(Width::Percentage(13)), // Platform
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Mode
            ColumnConstraint::Absolute(Width::Percentage(13)), // Layers
            ColumnConstraint::Absolute(Width::Percentage(10)), // Persist
            ColumnConstraint::Absolute(Width::Percentage(13)), // Ports
            ColumnConstraint::Absolute(Width::Percentage(9)),  // Mounts
            ColumnConstraint::Absolute(Width::Percentage(10)), // Depends
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Health
            ColumnConstraint::Absolute(Width::Percentage(6)),  // GPU
        ]);
    t.set_header(vec![
        Cell::new("Service")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Platform")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Mode")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Layers")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Persist")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Ports")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Mounts")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Depends")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Health")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("GPU")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);

    for svc in &m.services {
        let depends = if svc.depends_on.is_empty() {
            "—".to_string()
        } else {
            svc.depends_on.join(", ")
        };
        let gpu_label = match &svc.gpu {
            chefer_bundle::GpuRequest::Devices(d) if !d.is_empty() => format!(
                "cards {}",
                d.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
            ),
            g if g.enabled() => "yes".to_string(),
            _ => "no".to_string(),
        };
        t.add_row(vec![
            Cell::new(&svc.name).fg(Color::Cyan),
            Cell::new(&svc.platform),
            Cell::new(format!("{:?}", svc.interface_mode)).fg(Color::Magenta),
            Cell::new(format_layers(&svc.layers)).fg(Color::Yellow),
            Cell::new(svc.persist_path.as_deref().unwrap_or("—")).fg(Color::Yellow),
            Cell::new(format_ports(&svc.ports)).fg(Color::Blue),
            Cell::new(format_mounts(&svc.mounts)).fg(Color::Blue),
            Cell::new(depends).fg(Color::Magenta),
            Cell::new(if svc.healthcheck.is_some() {
                "yes"
            } else {
                "no"
            }),
            Cell::new(&gpu_label).fg(if svc.gpu.enabled() {
                Color::Green
            } else {
                Color::Reset
            }),
        ]);
    }

    println!("{}", "▎Services (manifest)".bold());
    println!("{t}");
    println!();

    render_companions(companions);
}

fn format_layers(layers: &[chefer_bundle::LayerRef]) -> String {
    if layers.is_empty() {
        return "0".to_string();
    }
    let total: u64 = layers.iter().filter_map(|l| l.size).sum();
    let mut parts = vec![format!(
        "{} total ({})",
        layers.len(),
        ui::human_bytes(total)
    )];
    for (idx, layer) in layers.iter().enumerate() {
        let size = layer
            .size
            .map(ui::human_bytes)
            .unwrap_or_else(|| "unknown size".to_string());
        parts.push(format!("#{idx}: {size}"));
    }
    parts.join("\n")
}

fn format_ports(ports: &[chefer_bundle::PortSpec]) -> String {
    if ports.is_empty() {
        "—".to_string()
    } else {
        ports
            .iter()
            .map(|p| format!("{}:{}/{}", p.host, p.guest, p.proto))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_mounts(mounts: &[chefer_bundle::MountSpec]) -> String {
    if mounts.is_empty() {
        "—".to_string()
    } else {
        mounts
            .iter()
            .map(|m| {
                format!(
                    "{}:{} ({})",
                    m.host,
                    m.guest,
                    if m.read_only { "ro" } else { "rw" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn render_companions(companions: &CompanionInventory) {
    let mut t = ui::kv_table(24, 76);
    t.add_row(vec![
        Cell::new("agents/").fg(Color::Cyan),
        Cell::new(if companions.agents.is_empty() {
            "—".to_string()
        } else {
            companions.agents.join(", ")
        }),
    ]);
    t.add_row(vec![
        Cell::new("vm/").fg(Color::Cyan),
        Cell::new(if companions.vm.is_empty() {
            "—".to_string()
        } else {
            companions.vm.join(", ")
        }),
    ]);
    println!("{}", "▎Embedded companion files".bold());
    println!("{t}");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chefer_bundle::{AppMeta, CrashPolicy, MANIFEST_FORMAT_VERSION, Manifest, NetworkMode};

    /// 建一個最小 bundle + 假 runtime，用 chefer-assembler 組成單檔。
    fn make_single_file(dir: &Path) -> std::path::PathBuf {
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let m = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            app: AppMeta {
                name: "InspectMe".into(),
                app_version: Some("1.2.3".into()),
                spec_version: "0.1".into(),
                old_names: vec![],
                data_dir_override: None,
                crash: CrashPolicy::FailFast,
                network: NetworkMode::Shared,
                console: chefer_bundle::ConsoleMode::Auto,
                generated_at_utc: "2026-01-01T00:00:00Z".into(),
                builder_version: "1.2.3".into(),
            },
            services: vec![chefer_bundle::ServiceEntry {
                name: "web".into(),
                platform: "linux/amd64".into(),
                layers: vec![chefer_bundle::LayerRef {
                    rel_path: "services/web/layers/0000-aaaaaaaaaaaa.tar.zst".into(),
                    diff_id:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .into(),
                    size: Some(2048),
                }],
                image_config: chefer_bundle::ImageConfig::default(),
                cmd_override: None,
                env: Default::default(),
                workdir_override: None,
                persist_path: Some("/data".into()),
                ports: vec![chefer_bundle::PortSpec {
                    host: 18080,
                    guest: 8080,
                    proto: chefer_bundle::PortProto::Tcp,
                }],
                mounts: vec![chefer_bundle::MountSpec {
                    host: "/host".into(),
                    guest: "/mnt/host".into(),
                    read_only: true,
                }],
                interface_mode: chefer_bundle::InterfaceMode::None,
                depends_on: vec!["db".into()],
                healthcheck: Some(chefer_bundle::HealthCheck {
                    test: chefer_bundle::CmdSpec::Argv(vec!["true".into()]),
                    interval_ms: 1000,
                    timeout_ms: 1000,
                    retries: 1,
                    start_period_ms: 0,
                }),
                gpu: Default::default(),
            }],
        };
        m.save(&chefer_bundle::layout::manifest_path(&bundle))
            .unwrap();
        std::fs::create_dir_all(bundle.join("agents")).unwrap();
        std::fs::write(bundle.join("agents/guest-agent-x86_64"), b"agent").unwrap();
        std::fs::write(bundle.join("agents/pasta-x86_64"), b"pasta").unwrap();
        std::fs::create_dir_all(bundle.join("vm")).unwrap();
        std::fs::write(bundle.join("vm/chefer-vmlinuz-x86_64"), b"kernel").unwrap();
        std::fs::write(bundle.join("vm/chefer-initramfs-x86_64"), b"initramfs").unwrap();
        // 額外放一個大一點的檔，確認 inspect 只取 manifest 也能正確跳過其他 entry
        std::fs::create_dir_all(bundle.join("services/web/layers")).unwrap();
        std::fs::write(
            bundle.join("services/web/layers/0000-aaaaaaaaaaaa.tar.zst"),
            vec![0u8; 300_000],
        )
        .unwrap();

        let runtime = dir.join("fake-runtime.exe");
        std::fs::write(&runtime, b"not really an exe but enough for assemble").unwrap();

        let out = dir.join("single.exe");
        chefer_assembler::assemble(
            &runtime,
            &bundle,
            &out,
            &chefer_assembler::AssembleOptions::default(),
        )
        .unwrap();
        out
    }

    #[test]
    fn reads_manifest_from_assembled_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let single = make_single_file(tmp.path());
        let footer = chefer_bundle::Footer::read_from_file(&single).unwrap();
        let payload = read_embedded_payload(&single, &footer).unwrap();
        assert_eq!(payload.manifest.app.name, "InspectMe");
        assert_eq!(payload.manifest.app.app_version.as_deref(), Some("1.2.3"));
        assert!(
            payload
                .companions
                .agents
                .contains(&"guest-agent-x86_64".to_string()),
            "應列出內嵌 guest-agent"
        );
        assert!(
            payload
                .companions
                .vm
                .contains(&"chefer-vmlinuz-x86_64".to_string()),
            "應列出內嵌 appliance"
        );
    }

    #[test]
    fn non_chefer_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("random.bin");
        std::fs::write(&p, vec![7u8; 500]).unwrap();
        let err = chefer_bundle::Footer::read_from_file(&p).unwrap_err();
        assert!(err.to_string().contains("magic"), "{err:#}");
    }

    #[test]
    fn cmd_inspect_end_to_end_prints_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let single = make_single_file(tmp.path());
        cmd_inspect(&single).unwrap();
    }

    /// 解壓炸彈：GNU 長檔名標頭宣告超大 size + 零位元組內容，
    /// 壓縮後僅數 KB，解壓會膨脹到數 GB。限制器必須在 OOM 前報錯。
    #[test]
    fn rejects_decompression_bomb_via_long_name_header() {
        use std::io::Write;

        // 手刻一個 GNU 長檔名（'L'）entry，宣告 size 略高於 inspect 上限，內容為零。
        // 真實攻擊可宣告數 GB；此處取剛好超過上限即足以證明限制器在 OOM 前生效。
        let bomb_size: u64 = chefer_bundle::bomb_limit_for(1) + 16 * 1024 * 1024;
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::GNULongName);
        h.set_mode(0);
        h.set_size(bomb_size);
        h.set_cksum();
        let mut tar_buf = Vec::new();
        tar_buf.extend_from_slice(h.as_bytes());
        // body：4 GiB 的零，但用 zstd 壓縮後極小。直接餵零給 encoder，不在記憶體展開。
        let payload = {
            let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
            enc.write_all(&tar_buf).unwrap();
            // 串流寫入零 body（分塊，避免在測試端配置 4GiB）
            let zeros = vec![0u8; 1024 * 1024];
            let mut remaining = bomb_size;
            while remaining > 0 {
                let n = remaining.min(zeros.len() as u64) as usize;
                enc.write_all(&zeros[..n]).unwrap();
                remaining -= n as u64;
            }
            enc.finish().unwrap()
        };

        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("bomb.exe");
        let sha: [u8; 32] = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&payload).into()
        };
        let ft = chefer_bundle::Footer::new_zstd(8, payload.len() as u64, sha);
        {
            let mut f = std::fs::File::create(&exe).unwrap();
            f.write_all(&[0xABu8; 8]).unwrap();
            f.write_all(&payload).unwrap();
            f.write_all(&ft.to_bytes()).unwrap();
        }

        // 限制器須在配置數 GB 之前回錯，而非 OOM。
        let err = read_embedded_payload(&exe, &ft).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds the limit") || msg.contains("manifest.json"),
            "應因超過解壓上限而報錯：{msg}"
        );
    }

    #[test]
    fn formatting_helpers_include_requested_details() {
        let layers = vec![chefer_bundle::LayerRef {
            rel_path: "services/web/layers/0000-a.tar.zst".into(),
            diff_id: "sha256:a".into(),
            size: Some(2048),
        }];
        assert!(format_layers(&layers).contains("#0: 2.00 KiB"));

        let ports = vec![chefer_bundle::PortSpec {
            host: 18080,
            guest: 8080,
            proto: chefer_bundle::PortProto::Tcp,
        }];
        assert_eq!(format_ports(&ports), "18080:8080/tcp");

        let mounts = vec![chefer_bundle::MountSpec {
            host: "/host".into(),
            guest: "/guest".into(),
            read_only: true,
        }];
        assert_eq!(format_mounts(&mounts), "/host:/guest (ro)");
    }
}
