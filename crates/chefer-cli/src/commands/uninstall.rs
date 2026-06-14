//! `chefer uninstall` — 移除 chefer 本身（CLI 執行檔 + 同層的 kit/）。
//!
//! 只移除 chefer 工具本身；**不會**動到：
//! - 你用 chefer 打包出來的 app 單檔（那是獨立檔案）
//! - 那些 app 執行後寫的持久化資料（在各平台資料目錄下，屬於 app 不屬於 chefer）
//! - Windows 上 chefer 建立的 WSL distro（chefer-rt-*）——僅提示如何手動移除
//!
//! 設計上不依賴任何外部狀態：以 self-replace 的 self_delete 刪掉自身執行檔
//! （Windows 上會排程於行程結束後刪除），並盡力清掉安裝腳本加進 PATH 的設定。

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

pub fn cmd_uninstall(yes: bool) -> Result<()> {
    let exe = std::env::current_exe().context("取得 chefer 執行檔路徑失敗")?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("無法判定 chefer 安裝目錄：{}", exe.display()))?
        .to_path_buf();

    // 同層的 kit：只有「看起來像 chefer kit」才移除（含 chefer-runtime-* 或 guest-agent-*），
    // 避免誤刪安裝目錄下剛好叫 kit 的其他東西。
    let kit = install_dir.join("kit");
    let kit_is_chefer = looks_like_chefer_kit(&kit);

    println!("{}", "將移除 chefer 本身：".bold());
    println!("  - 執行檔：{}", exe.display());
    if kit_is_chefer {
        println!("  - kit：   {}", kit.display());
    }
    println!(
        "{}",
        "不會移除：你打包出的 app 單檔、app 的持久化資料、WSL distro。".dimmed()
    );

    if !yes && !confirm("確定要移除 chefer 嗎？[y/N] ")? {
        println!("已取消。");
        return Ok(());
    }

    // 1) 先移除 kit（自身執行檔最後刪，避免中途失敗留下半套）
    if kit_is_chefer {
        fs::remove_dir_all(&kit)
            .with_context(|| format!("移除 kit 失敗：{}（可手動刪除）", kit.display()))?;
        println!("{} {}", "已移除".green(), kit.display());
    }

    // 2) 盡力清掉安裝腳本加進 PATH 的設定（找不到就只提示，不視為失敗）
    clean_path_entries(&install_dir);

    // 3) 最後刪掉自身執行檔
    //    - Unix：unlink 後本行程仍以已開啟的 inode 繼續跑到結束。
    //    - Windows：排程於行程結束後刪除（執行中無法直接刪）。
    self_replace::self_delete()
        .with_context(|| format!("移除 chefer 執行檔失敗：{}；可手動刪除該檔", exe.display()))?;
    println!("{} {}", "已移除".green(), exe.display());

    println!();
    println!("{}", "✔ chefer 已移除。".green().bold());
    println!(
        "{}",
        "提示：若 `chefer` 仍可呼叫，請開新終端機（PATH 變更需重載）。".dimmed()
    );
    #[cfg(windows)]
    println!(
        "{}",
        "提示：chefer 在 Windows 建立的 WSL distro 名為 chefer-rt-*；\
         要一併清除可執行 `wsl --unregister <name>`（不清也不影響系統）。"
            .dimmed()
    );
    println!(
        "{}",
        "提示：你打包的 app 寫的資料未被移除（屬於 app，不屬於 chefer）。".dimmed()
    );
    Ok(())
}

/// kit 目錄是否像 chefer 的 kit（含 chefer-runtime-* 或 guest-agent-* 檔）。
fn looks_like_chefer_kit(kit: &Path) -> bool {
    let Ok(rd) = fs::read_dir(kit) else {
        return false;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("chefer-runtime-") || name.starts_with("guest-agent-") {
            return true;
        }
    }
    false
}

/// 詢問使用者 y/N；非互動（無 stdin）時視為否。
fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    let n = io::stdin()
        .read_line(&mut line)
        .context("讀取確認輸入失敗")?;
    if n == 0 {
        return Ok(false); // EOF / 非互動
    }
    let a = line.trim().to_ascii_lowercase();
    Ok(a == "y" || a == "yes")
}

/// 盡力移除安裝腳本加到 PATH 的設定（best-effort；任何失敗只提示不致命）。
fn clean_path_entries(install_dir: &Path) {
    #[cfg(windows)]
    {
        // 安裝腳本把 install_dir 加進「使用者 PATH」（登錄檔）。以 powershell 過濾移除；
        // 目錄字串以環境變數傳入，避免命令注入。指令本身為純 ASCII。
        let script = "$d=$env:CHEFER_RM_DIR; \
             $p=[Environment]::GetEnvironmentVariable('Path','User'); \
             if ($p) { $n=($p -split ';' | Where-Object { $_ -and ($_ -ne $d) }) -join ';'; \
             [Environment]::SetEnvironmentVariable('Path',$n,'User') }";
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("CHEFER_RM_DIR", install_dir)
            .status();
        match status {
            Ok(s) if s.success() => {
                println!(
                    "{} 使用者 PATH 中的 {} 條目",
                    "已清除".green(),
                    install_dir.display()
                )
            }
            _ => println!(
                "{}",
                format!(
                    "提示：請自行從使用者 PATH 移除 {}（自動清除未成功）。",
                    install_dir.display()
                )
                .dimmed()
            ),
        }
    }
    #[cfg(not(windows))]
    {
        // 安裝腳本在 rc 檔附加了：
        //   # chefer
        //   export PATH="<install_dir>:$PATH"
        // 移除含 install_dir 的那行 export 與其上方的 `# chefer` 標記行。
        let needle = install_dir.to_string_lossy().to_string();
        let mut cleaned_any = false;
        if let Some(home) = std::env::var_os("HOME") {
            let home = Path::new(&home);
            for rc in [".profile", ".bashrc", ".zshrc"] {
                let path = home.join(rc);
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                let lines: Vec<&str> = content.lines().collect();
                let mut out: Vec<&str> = Vec::with_capacity(lines.len());
                let mut removed = false;
                let mut i = 0;
                while i < lines.len() {
                    let l = lines[i];
                    let is_chefer_path = l.contains("export PATH=") && l.contains(&needle);
                    if is_chefer_path {
                        // 一併吃掉緊鄰其上的 `# chefer` 標記行
                        if out.last().map(|p| p.trim()) == Some("# chefer") {
                            out.pop();
                        }
                        removed = true;
                        i += 1;
                        continue;
                    }
                    out.push(l);
                    i += 1;
                }
                if removed {
                    let mut new_content = out.join("\n");
                    if content.ends_with('\n') {
                        new_content.push('\n');
                    }
                    if fs::write(&path, new_content).is_ok() {
                        cleaned_any = true;
                        println!("{} {}", "已清除 PATH 設定".green(), path.display());
                    }
                }
            }
        }
        if !cleaned_any {
            println!(
                "{}",
                format!(
                    "提示：若有手動把 {} 加進 PATH，請自行移除。",
                    install_dir.display()
                )
                .dimmed()
            );
        }
    }
}
