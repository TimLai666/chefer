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
        Shared => "shared（共用 host 網路，未宣告的 port 也可達）",
        Internal => "internal（隔離；只開宣告的 port，無對外網路）",
        Bridge => "bridge（隔離；只開宣告的 port，可出網）",
    }
}

/// 解壓輸出總量上限：bundle 佈局中 manifest.json 之前只有 agents/（數 MB）
/// 與 appcipe.yml，256 MiB 足以涵蓋並讀到 manifest，遠低於 OOM 門檻。
/// 套在 tar 之前以擋下長檔名/pax 標頭撐爆 read_to_end 的解壓炸彈。
const MAX_INSPECT_INFLATE_BYTES: u64 = 256 * 1024 * 1024;

pub fn cmd_inspect(file: &Path) -> Result<()> {
    let footer = chefer_bundle::Footer::read_from_file(file)?;
    let file_size = std::fs::metadata(file)
        .with_context(|| format!("讀取檔案中繼資料失敗：{}", file.display()))?
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
            ui::human_mb(footer.offset)
        )),
    ]);
    t.add_row(vec![
        Cell::new("Payload Size").fg(Color::Cyan),
        Cell::new(format!(
            "{} bytes ({})",
            footer.length,
            ui::human_mb(footer.length)
        )),
    ]);
    t.add_row(vec![
        Cell::new("Payload SHA-256").fg(Color::Cyan),
        Cell::new(ui::hex(&footer.sha256)),
    ]);
    t.add_row(vec![
        Cell::new("File Size").fg(Color::Cyan),
        Cell::new(format!("{} bytes ({})", file_size, ui::human_mb(file_size))),
    ]);

    println!();
    println!("{}", "▎Footer".bold());
    println!("{t}");

    // ── 串流取出 manifest.json 並列印摘要 ───────────────────────
    let manifest = read_embedded_manifest(file, &footer)?;
    render_manifest_summary(&manifest);
    Ok(())
}

/// 串流讀取 payload，只取出 `bundle/manifest.json`（不解壓其他 entry 到磁碟）。
fn read_embedded_manifest(
    path: &Path,
    footer: &chefer_bundle::Footer,
) -> Result<chefer_bundle::Manifest> {
    if !footer.is_zstd() {
        bail!(
            "不支援的 payload 格式（flags={:#04x}）：本版僅支援 zstd 壓縮的 tar payload",
            footer.flags
        );
    }

    let mut f =
        std::fs::File::open(path).with_context(|| format!("開啟檔案失敗：{}", path.display()))?;
    f.seek(SeekFrom::Start(footer.offset))
        .context("移動到 payload 起點失敗")?;
    let limited = BufReader::new(f).take(footer.length);
    let decoder = zstd::stream::read::Decoder::new(limited)
        .context("建立 zstd 解碼器失敗（payload 可能損毀）")?;
    // 在 tar 之前限制解壓「輸出」總量：擋下長檔名/pax 標頭撐爆 read_to_end 的炸彈，
    // 以及把 manifest 排在超大 entry 之後的 DoS。take(footer.length) 只限壓縮輸入，無法防此。
    let guarded = chefer_bundle::LimitedReader::new(decoder, MAX_INSPECT_INFLATE_BYTES);
    let mut archive = tar::Archive::new(guarded);

    for entry in archive.entries().context("讀取 payload tar 失敗")? {
        let mut entry = entry.context("讀取 payload tar entry 失敗")?;
        let entry_path = entry
            .path()
            .context("讀取 tar entry 路徑失敗")?
            .to_string_lossy()
            .replace('\\', "/");
        if entry_path != "bundle/manifest.json" {
            continue; // 其他 entry 一律跳過（iterator 會自行略過內容，不落地）
        }
        if entry.size() > MAX_MANIFEST_BYTES {
            bail!(
                "manifest.json 過大（{} bytes，上限 {} bytes）；檔案可能已損毀",
                entry.size(),
                MAX_MANIFEST_BYTES
            );
        }
        let mut s = String::new();
        entry
            .read_to_string(&mut s)
            .context("讀取 manifest.json 內容失敗")?;
        let m: chefer_bundle::Manifest =
            serde_json::from_str(&s).context("解析內嵌 manifest.json 失敗")?;
        if m.format_version != chefer_bundle::MANIFEST_FORMAT_VERSION {
            bail!(
                "不支援的 manifest 格式版本 {}（本版支援 {}）；請以相同版本的 chefer 重新打包",
                m.format_version,
                chefer_bundle::MANIFEST_FORMAT_VERSION
            );
        }
        return Ok(m);
    }
    bail!("payload 內找不到 bundle/manifest.json；檔案可能不是 Chefer 單檔或已損毀");
}

/// 列印 manifest 的 app 與 services 摘要表格。
fn render_manifest_summary(m: &chefer_bundle::Manifest) {
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
            ColumnConstraint::Absolute(Width::Percentage(12)), // Service
            ColumnConstraint::Absolute(Width::Percentage(14)), // Platform
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Layers
            ColumnConstraint::Absolute(Width::Percentage(12)), // Size
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Mode
            ColumnConstraint::Absolute(Width::Percentage(18)), // Persist
            ColumnConstraint::Absolute(Width::Percentage(16)), // Ports
            ColumnConstraint::Absolute(Width::Percentage(12)), // Depends
        ]);
    t.set_header(vec![
        Cell::new("Service")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Platform")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Layers")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Size")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Mode")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Persist")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Ports")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Depends")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);

    for svc in &m.services {
        let layers_size: u64 = svc.layers.iter().filter_map(|l| l.size).sum();
        let ports = if svc.ports.is_empty() {
            "—".to_string()
        } else {
            svc.ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let depends = if svc.depends_on.is_empty() {
            "—".to_string()
        } else {
            svc.depends_on.join(", ")
        };
        t.add_row(vec![
            Cell::new(&svc.name).fg(Color::Cyan),
            Cell::new(&svc.platform),
            Cell::new(svc.layers.len()),
            Cell::new(ui::human_mb(layers_size)).fg(Color::Yellow),
            Cell::new(format!("{:?}", svc.interface_mode)).fg(Color::Magenta),
            Cell::new(svc.persist_path.as_deref().unwrap_or("—")).fg(Color::Yellow),
            Cell::new(ports).fg(Color::Blue),
            Cell::new(depends).fg(Color::Magenta),
        ]);
    }

    println!("{}", "▎Services (manifest)".bold());
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
                generated_at_utc: "2026-01-01T00:00:00Z".into(),
                builder_version: "1.2.3".into(),
            },
            services: vec![],
        };
        m.save(&chefer_bundle::layout::manifest_path(&bundle))
            .unwrap();
        // 額外放一個大一點的檔，確認 inspect 只取 manifest 也能正確跳過其他 entry
        std::fs::create_dir_all(bundle.join("services/db/layers")).unwrap();
        std::fs::write(
            bundle.join("services/db/layers/0000-aaaaaaaaaaaa.tar.zst"),
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
        let m = read_embedded_manifest(&single, &footer).unwrap();
        assert_eq!(m.app.name, "InspectMe");
        assert_eq!(m.app.app_version.as_deref(), Some("1.2.3"));
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
        let bomb_size: u64 = MAX_INSPECT_INFLATE_BYTES + 16 * 1024 * 1024;
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
        let err = read_embedded_manifest(&exe, &ft).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("超過上限") || msg.contains("manifest.json"),
            "應因超過解壓上限而報錯：{msg}"
        );
    }
}
