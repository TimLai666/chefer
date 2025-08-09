use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::{
    Attribute, Cell, Color, ColumnConstraint, ContentArrangement, Table, Width,
    presets::UTF8_BORDERS_ONLY,
};
use crossterm::terminal;
use owo_colors::OwoColorize;
use self_update::cargo_crate_version;

const REPO_OWNER: &str = "YOUR_GH_OWNER"; // ← 換成你的 GitHub owner
const REPO_NAME: &str = "YOUR_GH_REPO"; // ← 換成你的 GitHub repo
const BIN_NAME: &str = "chefer"; // 你的二進位檔名
const APPCIPE_SPEC_VERSION: &str = "0.1"; // 目前支援的 appcipe 規格版本

#[derive(Parser, Debug)]
#[command(
    name = "chefer",
    version,
    about = "Chefer — Cook Your Containers into Delicious Apps"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 讀取並驗證 appcipe.yml，依格式輸出摘要
    Check {
        /// 路徑：appcipe.yml
        file: String,

        /// 輸出格式：pretty/json/yaml
        #[arg(long, value_enum, default_value_t = PrintFmt::Pretty)]
        format: PrintFmt,
    },

    /// 依食譜建置（MVP 先放前置邏輯）
    Build {
        /// 路徑：appcipe.yml
        file: String,
        /// 只做檢查與前置，不輸出
        #[arg(long)]
        dry_run: bool,
    },

    /// 顯示 Chefer 與環境版本資訊
    Version,

    /// 自動更新到最新版（不依賴 cargo）
    Upgrade {
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        check_only: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PrintFmt {
    Pretty,
    Json,
    Yaml,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Check { file, format } => cmd_check(&file, format),
        Cmd::Build { file, dry_run } => cmd_build(&file, dry_run),
        Cmd::Version => cmd_version(),
        Cmd::Upgrade {
            channel,
            to,
            check_only,
        } => cmd_upgrade(&channel, to.as_deref(), check_only),
    }
}

fn cmd_check(file: &str, fmt: PrintFmt) -> Result<()> {
    let app = appcipe_spec::from_file(file).map_err(|e| anyhow!("{e}"))?;
    match fmt {
        PrintFmt::Pretty => {
            println!(
                "{}  {}",
                "✔ Verified".green().bold(),
                format!("{} v{}", app.name.blue().bold(), app.version)
            );
            render_summary_table(&app);
        }
        PrintFmt::Json => {
            let s = serde_json::to_string_pretty(&app)?;
            println!("{s}");
        }
        PrintFmt::Yaml => {
            let s = serde_yaml::to_string(&app)?;
            println!("{s}");
        }
    }
    Ok(())
}

fn cmd_build(file: &str, dry_run: bool) -> Result<()> {
    let app = appcipe_spec::from_file(file).map_err(|e| anyhow!("{e}"))?;
    println!(
        "{}  {}",
        "🔧 Prepare to build".yellow().bold(),
        format!("{} v{}", app.name.blue().bold(), app.version)
    );
    render_summary_table(&app);

    if dry_run {
        println!("{}", "（dry-run）僅前置檢查完成。".dimmed());
        return Ok(());
    }

    // 之後這裡會呼叫 chefer-pack：pack_all(&app, &out_dir)
    Err(anyhow!(
        "build 還沒實作（下一步會串 chefer-pack 解 tar → 合層 rootfs）"
    ))
}

fn cmd_version() -> Result<()> {
    use comfy_table::{Table, presets::UTF8_BORDERS_ONLY};

    // 這些環境變數由 build.rs 注入（若沒取到會編譯錯誤，所以請先加 build.rs）
    let chefer_ver = env!("CARGO_PKG_VERSION");
    let spec_ver = APPCIPE_SPEC_VERSION;
    let git_sha = env!("GIT_SHA");
    let build_time = env!("BUILD_TIME");
    let target = env!("BUILD_TARGET");

    let mut t = Table::new();
    t.load_preset(UTF8_BORDERS_ONLY);
    t.set_header(vec!["Key", "Value"]);
    t.add_row(vec!["Chefer", chefer_ver]);
    t.add_row(vec!["AppCipe Spec", spec_ver]);
    t.add_row(vec!["Target", target]);
    t.add_row(vec!["Git SHA", git_sha]);
    t.add_row(vec!["Build Time (UTC)", build_time]);

    println!("{t}");
    Ok(())
}

fn cmd_upgrade(channel: &str, to: Option<&str>, check_only: bool) -> Result<()> {
    use self_update::Status;
    use self_update::backends::github::Update;
    let repo_owner = "YourGitHubUser";
    let repo_name = "chefer";

    let mut builder = Update::configure();
    builder
        .repo_owner(repo_owner)
        .repo_name(repo_name)
        .bin_name("chefer")
        .show_download_progress(true)
        .current_version(cargo_crate_version!());

    if let Some(ver) = to {
        builder.target_version_tag(ver);
    }
    if channel != "stable" {
        builder.target_version_tag(channel);
    }

    if check_only {
        match builder.build()?.update()? {
            Status::UpToDate(ver) => {
                println!("{}  {}", "Already up to date".green().bold(), ver);
            }
            Status::Updated(ver) => {
                println!(
                    "{}  {} → {} (update available)",
                    "Update available".yellow().bold(),
                    cargo_crate_version!(),
                    ver
                );
            }
        }
        return Ok(());
    }

    match builder.build()?.update()? {
        Status::UpToDate(ver) => {
            println!("{}  {}", "Already up to date".green().bold(), ver);
        }
        Status::Updated(ver) => {
            println!(
                "{}  {} → {}",
                "✔ Updated".green().bold(),
                cargo_crate_version!(),
                ver
            );
        }
    }

    Ok(())
}

/* ---------- UI Helpers ---------- */
fn render_summary_table(app: &appcipe_spec::AppCipe) {
    let cols = terminal::size().map(|(c, _)| c).unwrap_or(120);

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
    header.add_row(vec![
        Cell::new("Spec Version").fg(Color::Cyan),
        Cell::new(&app.version),
    ]);
    header.add_row(vec![
        Cell::new("Crash Policy").fg(Color::Cyan),
        Cell::new(
            app.crash
                .as_ref()
                .map(|c| format!("{c:?}"))
                .unwrap_or_else(|| "default".to_string()),
        )
        .fg(Color::Yellow),
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

    /* ===== Services（百分比分配；單格換行；彩色；Service 間插入空白列） ===== */
    let mut t = Table::new();
    t.load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(cols)
        .set_constraints(vec![
            ColumnConstraint::Absolute(Width::Percentage(10)), // Service
            ColumnConstraint::Absolute(Width::Percentage(36)), // Image
            ColumnConstraint::Absolute(Width::Percentage(8)),  // Mode
            ColumnConstraint::Absolute(Width::Percentage(18)), // Persist
            ColumnConstraint::Absolute(Width::Percentage(12)), // Ports
            ColumnConstraint::Absolute(Width::Percentage(16)), // Mounts
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
    ]);

    let total = app.services.len();
    for (idx, (name, svc)) in app.services.iter().enumerate() {
        let image = match &svc.image {
            appcipe_spec::ImageSourceOrPath::TarPath(p) => format!("tar: {p}"),
            appcipe_spec::ImageSourceOrPath::Full {
                source,
                file,
                format,
                platform,
            } => {
                let mut s = format!("{:?}:{file}", source);
                if let Some(f) = format {
                    s.push_str(&format!(" ({f})"));
                }
                if let Some(pf) = platform {
                    s.push_str(&format!(" [{pf}]"));
                }
                s
            }
        };

        let mode = svc
            .interface_mode
            .as_ref()
            .map(|m| format!("{m:?}"))
            .unwrap_or_else(|| "default".to_string());

        let persist = svc.persist_path.as_deref().unwrap_or("—").to_string();
        let ports = svc
            .ports
            .as_ref()
            .map(|v| {
                if v.is_empty() {
                    "—".into()
                } else {
                    v.join(", ")
                }
            })
            .unwrap_or_else(|| "—".into());
        let mounts = svc
            .mounts
            .as_ref()
            .map(|v| {
                if v.is_empty() {
                    "—".into()
                } else {
                    v.join(", ")
                }
            })
            .unwrap_or_else(|| "—".into());

        t.add_row(vec![
            Cell::new(name).fg(Color::Cyan),
            Cell::new(image).fg(Color::White),
            Cell::new(mode).fg(Color::Magenta),
            Cell::new(persist).fg(Color::Yellow),
            Cell::new(ports).fg(Color::Blue),
            Cell::new(mounts).fg(Color::Blue),
        ]);

        // 在每個 service 後面插入一行空白列（最後一個不插）
        if idx + 1 < total {
            t.add_row(vec![
                Cell::new(""),
                Cell::new(""),
                Cell::new(""),
                Cell::new(""),
                Cell::new(""),
                Cell::new(""),
            ]);
        }
    }

    println!("{}", "▎Services".bold());
    println!("{t}");
    println!();
}
