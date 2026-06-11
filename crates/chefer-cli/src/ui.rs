//! 表格 UI 輔助：appcipe 摘要表格、雙欄 key-value 表格、大小格式化。
//! 沿用既有的 comfy-table + owo-colors 風格。

use comfy_table::{
    Attribute, Cell, Color, ColumnConstraint, ContentArrangement, Table, Width,
    presets::UTF8_BORDERS_ONLY,
};
use crossterm::terminal;
use owo_colors::OwoColorize;

/// 取得目前終端寬度（取不到時退回 120）。
pub fn term_width() -> u16 {
    terminal::size().map(|(c, _)| c).unwrap_or(120)
}

/// 建立雙欄 key-value 表格（欄寬以百分比指定），header 為 Key/Value。
pub fn kv_table(key_pct: u16, val_pct: u16) -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(term_width())
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(key_pct)),
            ColumnConstraint::Absolute(Width::Percentage(val_pct)),
        ]);
    t.set_header(vec![
        Cell::new("Key")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Value")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);
    t
}

/// 位元組數 → 人類可讀 MB 字串（固定 MB 單位，輸出穩定易比較）。
pub fn human_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// 位元組 slice → 小寫十六進位字串（顯示 sha256 用）。
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 渲染 appcipe 的 App 概覽與 Services 摘要表格（check / build 共用）。
pub fn render_summary_table(app: &appcipe_spec::AppCipe) {
    let cols = term_width();

    /* ===== App 概覽（兩欄，比例 24/76；允許單格換行；加點顏色） ===== */
    let mut header = Table::new();
    header
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(cols)
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(24)),
            ColumnConstraint::Absolute(Width::Percentage(76)),
        ]);

    header.set_header(vec![
        Cell::new("Field")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Value")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);

    header.add_row(vec![
        Cell::new("Name").fg(Color::Cyan),
        Cell::new(&app.name),
    ]);
    if let Some(ver) = &app.app_version {
        header.add_row(vec![
            Cell::new("App Version").fg(Color::Cyan),
            Cell::new(ver),
        ]);
    }
    header.add_row(vec![
        Cell::new("Spec Version").fg(Color::Cyan),
        Cell::new(&app.version),
    ]);
    header.add_row(vec![
        Cell::new("Crash Policy").fg(Color::Cyan),
        Cell::new(format!("{:?}", app.crash)).fg(Color::Yellow),
    ]);
    if let Some(dir) = &app.data_dir {
        header.add_row(vec![Cell::new("Data Dir").fg(Color::Cyan), Cell::new(dir)]);
    }
    if !app.old_names.is_empty() {
        header.add_row(vec![
            Cell::new("Old Names").fg(Color::Cyan),
            Cell::new(app.old_names.join(", ")).fg(Color::Magenta),
        ]);
    }

    println!();
    println!("{}", "▎App Information".bold());
    println!("{header}");
    println!();

    /* ===== Services（百分比分配；單格換行；彩色；含 Depends；Service 間插入空白列） ===== */
    let mut t = Table::new();
    t.load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(cols)
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(10)), // Service
            ColumnConstraint::Absolute(Width::Percentage(30)), // Image
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Mode
            ColumnConstraint::Absolute(Width::Percentage(16)), // Persist
            ColumnConstraint::Absolute(Width::Percentage(12)), // Ports
            ColumnConstraint::Absolute(Width::Percentage(16)), // Mounts
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Depends
        ]);

    t.set_header(vec![
        Cell::new("Service")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Image")
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
        Cell::new("Mounts")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
        Cell::new("Depends")
            .add_attribute(Attribute::Bold)
            .fg(Color::Green),
    ]);

    // 依名稱排序，輸出順序確定（HashMap 走訪順序不穩定）。
    let mut names: Vec<&String> = app.services.keys().collect();
    names.sort();

    let total = names.len();
    for (idx, name) in names.iter().enumerate() {
        let svc = &app.services[*name];
        let image = image_display(&svc.image);
        let mode = format!("{:?}", svc.interface_mode);
        let persist = svc.persist_path.as_deref().unwrap_or("—").to_string();
        let ports = if svc.ports.is_empty() {
            "—".into()
        } else {
            svc.ports.join(", ")
        };
        let mounts = if svc.mounts.is_empty() {
            "—".into()
        } else {
            svc.mounts.join(", ")
        };
        let depends = if svc.depends_on.is_empty() {
            "—".into()
        } else {
            svc.depends_on.join(", ")
        };

        t.add_row(vec![
            Cell::new(name).fg(Color::Cyan),
            Cell::new(image).fg(Color::White),
            Cell::new(mode).fg(Color::Magenta),
            Cell::new(persist).fg(Color::Yellow),
            Cell::new(ports).fg(Color::Blue),
            Cell::new(mounts).fg(Color::Blue),
            Cell::new(depends).fg(Color::Magenta),
        ]);

        if idx + 1 < total {
            t.add_row(vec![""; 7]);
        }
    }

    println!("{}", "▎Services".bold());
    println!("{t}");
    println!();
}

/// Image 欄顯示：以檔案路徑為主體；
/// 非預設 format 附註 `(docker-archive|oci-archive)`、非預設 platform 附註 `[linux/arm64]`，
/// 非 tar 來源（dockerfile/image，目前驗證不允許，僅防禦性顯示）加上 `source:` 前綴。
pub fn image_display(image: &appcipe_spec::ImageSourceOrPath) -> String {
    match image {
        appcipe_spec::ImageSourceOrPath::TarPath(p) => p.clone(),
        appcipe_spec::ImageSourceOrPath::Full {
            source,
            file,
            format,
            platform,
        } => {
            let mut s = match source {
                appcipe_spec::ImageSourceType::Tar => file.clone(),
                appcipe_spec::ImageSourceType::Dockerfile => format!("dockerfile:{file}"),
                appcipe_spec::ImageSourceType::Image => format!("image:{file}"),
            };
            match format {
                appcipe_spec::ImageFormat::Auto => {}
                appcipe_spec::ImageFormat::DockerArchive => s.push_str(" (docker-archive)"),
                appcipe_spec::ImageFormat::OciArchive => s.push_str(" (oci-archive)"),
            }
            match platform {
                appcipe_spec::ImagePlatform::LinuxAmd64 => {}
                appcipe_spec::ImagePlatform::LinuxArm64 => s.push_str(" [linux/arm64]"),
                appcipe_spec::ImagePlatform::WindowsAmd64 => s.push_str(" [windows/amd64]"),
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_mb_formats() {
        assert_eq!(human_mb(0), "0.00 MB");
        assert_eq!(human_mb(1024 * 1024), "1.00 MB");
        assert_eq!(human_mb(1024 * 1024 * 1024), "1024.00 MB");
    }

    #[test]
    fn hex_formats() {
        assert_eq!(hex(&[0x00, 0xff, 0x0a]), "00ff0a");
    }

    #[test]
    fn image_display_variants() {
        use appcipe_spec::{ImageFormat, ImagePlatform, ImageSourceOrPath, ImageSourceType};
        let short = ImageSourceOrPath::TarPath("./db.tar".into());
        assert_eq!(image_display(&short), "./db.tar");

        let full = ImageSourceOrPath::Full {
            source: ImageSourceType::Tar,
            file: "./db.tar".into(),
            format: ImageFormat::OciArchive,
            platform: ImagePlatform::LinuxArm64,
        };
        assert_eq!(image_display(&full), "./db.tar (oci-archive) [linux/arm64]");

        let plain = ImageSourceOrPath::Full {
            source: ImageSourceType::Tar,
            file: "./db.tar".into(),
            format: ImageFormat::Auto,
            platform: ImagePlatform::LinuxAmd64,
        };
        assert_eq!(image_display(&plain), "./db.tar");
    }
}
