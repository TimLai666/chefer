//! rootless subuid/subgid 委派（docs/DESIGN.md §6「網路隔離」／rootless 限制）。
//!
//! 非 root 執行時，`unshare(NEWUSER)` 後行程自寫 uid_map 只能映射單一 uid——會
//! `chown`/`gosu` 到其他服務 uid 的映像（官方 redis/postgres 的 999…）因此失敗。
//! 範圍映射必須由仍在 parent user ns、具特權的行程代寫：標準做法是 setuid-root 的
//! `newuidmap`/`newgidmap`（shadow-utils），依 `/etc/subuid`/`/etc/subgid` 對呼叫者
//! 的委派範圍驗證後代寫 `/proc/<pid>/[ug]id_map`——與 rootless podman 同一機制。
//!
//! 本模組負責「偵測 + 套用」：
//! - [`delegation()`]：解析呼叫者在 `/etc/subuid`/`/etc/subgid` 的範圍並定位
//!   `newuidmap`/`newgidmap`，兩者齊備才回 `Some`（process 級快取，附一次性記錄）。
//! - [`Delegation::apply(pid)`]：對剛 `unshare(NEWUSER)` 的子行程代寫範圍映射
//!   （`0 <uid> 1` + 從容器 uid 1 起依序接上各委派範圍，podman 慣例）。
//!
//! 套用點見 `exec.rs`（shared 模式 middle 行程）與 `netns.rs`（internal/bridge 的
//! netns holder；服務加入 holder 的 user ns 自動繼承映射）。任一步不可用/失敗都
//! 退回既有的單一 uid 自寫映射，不致命。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// 呼叫者可用的 subuid/subgid 委派（偵測結果，process 級不變）。
pub struct Delegation {
    newuidmap: PathBuf,
    newgidmap: PathBuf,
    /// newuidmap 的映射參數三元組（container_id, host_id, count），首項為 `0 <uid> 1`。
    uid_triples: Vec<(u32, u32, u32)>,
    gid_triples: Vec<(u32, u32, u32)>,
}

impl Delegation {
    /// 對 `pid`（剛 `unshare(NEWUSER)`、尚未寫映射的子行程）代寫範圍 uid/gid map。
    /// gid 不需先寫 setgroups deny（那是無特權自寫的限制）→ 容器內 setgroups 可用，
    /// `gosu` 這類會呼叫 setgroups 的工具才能動。
    pub fn apply(&self, pid: i32) -> std::io::Result<()> {
        run_map_tool(&self.newuidmap, pid, &self.uid_triples)?;
        run_map_tool(&self.newgidmap, pid, &self.gid_triples)?;
        Ok(())
    }

    /// 容器內可用的 uid 總數（診斷訊息用）。
    fn uid_span(&self) -> u32 {
        self.uid_triples.iter().map(|t| t.2).sum()
    }
}

fn run_map_tool(tool: &Path, pid: i32, triples: &[(u32, u32, u32)]) -> std::io::Result<()> {
    let mut cmd = Command::new(tool);
    cmd.arg(pid.to_string());
    for (inner, outer, count) in triples {
        cmd.arg(inner.to_string())
            .arg(outer.to_string())
            .arg(count.to_string());
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{} exited with {status}",
            tool.display()
        )));
    }
    Ok(())
}

/// 偵測目前使用者的 subuid/subgid 委派；不可用回 `None`。
/// 只在非 root 呼叫端有意義（root 後端不開 user ns、不需委派）。
/// 結果 process 級快取；首次呼叫**必須在 supervisor（fork 前）**發生，順帶印一次診斷。
pub fn delegation() -> Option<&'static Delegation> {
    static CACHE: OnceLock<Option<Delegation>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let d = detect();
            match &d {
                Some(d) => eprintln!(
                    "[guest-agent] rootless: /etc/subuid delegation active \
                     ({} uids mapped via newuidmap); chown/gosu images are supported",
                    d.uid_span()
                ),
                None => eprintln!(
                    "[guest-agent] rootless: no /etc/subuid delegation (newuidmap/newgidmap \
                     not found, or no subuid/subgid ranges for this user) → single-uid map; \
                     images that chown/gosu to another uid may fail"
                ),
            }
            d
        })
        .as_ref()
}

fn detect() -> Option<Delegation> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    // 取使用者名稱供 /etc/subuid 比對（找不到帳號時仍可用數字 uid 比對）。
    let name = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name);
    let newuidmap = find_in_path("newuidmap")?;
    let newgidmap = find_in_path("newgidmap")?;
    let sub_uids = parse_subid_ranges(
        &std::fs::read_to_string("/etc/subuid").ok()?,
        name.as_deref(),
        uid,
    );
    let sub_gids = parse_subid_ranges(
        &std::fs::read_to_string("/etc/subgid").ok()?,
        name.as_deref(),
        gid,
    );
    if sub_uids.is_empty() || sub_gids.is_empty() {
        return None;
    }
    Some(Delegation {
        newuidmap,
        newgidmap,
        uid_triples: build_triples(uid, &sub_uids),
        gid_triples: build_triples(gid, &sub_gids),
    })
}

/// 解析 subuid/subgid 內容中屬於 `name`（或數字 `id`）的所有範圍（`名稱:起點:數量`）。
/// 依 shadow-utils 慣例，同一使用者可有多行；全部收集、順序保留。
fn parse_subid_ranges(content: &str, name: Option<&str>, id: u32) -> Vec<(u32, u32)> {
    let id_str = id.to_string();
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(3, ':');
        let (Some(owner), Some(start), Some(count)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if Some(owner) != name && owner != id_str {
            continue;
        }
        if let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.trim().parse::<u32>())
            && count > 0
        {
            out.push((start, count));
        }
    }
    out
}

/// 組 newuidmap/newgidmap 參數：容器 id 0 ↔ 呼叫者自身，容器 id 1 起依序接上各委派範圍。
fn build_triples(self_id: u32, ranges: &[(u32, u32)]) -> Vec<(u32, u32, u32)> {
    let mut triples = vec![(0u32, self_id, 1u32)];
    let mut next_inner = 1u32;
    for &(start, count) in ranges {
        triples.push((next_inner, start, count));
        next_inner = next_inner.saturating_add(count);
    }
    triples
}

fn find_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 委派握手的管線組（fork 前由 supervisor 建立；CLOEXEC，服務 exec 後不外洩）。
/// 協定：子行程 `unshare(NEWUSER)` 後寫 1 byte 到 `unshared`，supervisor 收到後以
/// newuidmap/newgidmap 對子行程代寫範圍映射，再回 1 byte verdict——`b'M'`＝已代寫、
/// `b'S'`＝代寫失敗請自寫單一映射（fallback，不致命）。
pub struct MapSync {
    pub unshared_r: std::os::fd::OwnedFd,
    pub unshared_w: std::os::fd::OwnedFd,
    pub verdict_r: std::os::fd::OwnedFd,
    pub verdict_w: std::os::fd::OwnedFd,
}

impl MapSync {
    pub fn new() -> std::io::Result<Self> {
        let (unshared_r, unshared_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        let (verdict_r, verdict_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        Ok(Self {
            unshared_r,
            unshared_w,
            verdict_r,
            verdict_w,
        })
    }
}

/// 子行程側：`unshare(NEWUSER)` 之後呼叫——通知 supervisor 並等 verdict。
/// 回 `true`＝supervisor 已代寫範圍映射（跳過自寫）；`false`＝請自寫單一映射。
pub fn child_wait_maps(
    unshared_w: &std::os::fd::OwnedFd,
    verdict_r: &std::os::fd::OwnedFd,
) -> bool {
    let _ = nix::unistd::write(unshared_w, b"U");
    let mut buf = [0u8; 1];
    matches!(nix::unistd::read(verdict_r, &mut buf), Ok(1) if buf[0] == b'M')
}

/// supervisor 側：等子行程 unshare 完成，套範圍映射並回 verdict。
/// 子行程提前死亡（EOF）或映射失敗都以 `b'S'` 收尾（子行程自寫 fallback）。
pub fn supervisor_write_maps(
    d: &Delegation,
    child: nix::unistd::Pid,
    unshared_r: &std::os::fd::OwnedFd,
    verdict_w: &std::os::fd::OwnedFd,
) {
    let mut buf = [0u8; 1];
    let verdict = match nix::unistd::read(unshared_r, &mut buf) {
        Ok(1) => match d.apply(child.as_raw()) {
            Ok(()) => b'M',
            Err(e) => {
                eprintln!(
                    "[guest-agent] rootless: newuidmap/newgidmap failed for pid {} ({e}); \
                     falling back to a single-uid map",
                    child.as_raw()
                );
                b'S'
            }
        },
        // EOF/錯誤：子行程已死或管線壞了；回 'S' 盡力收尾（寫入端可能也已無人讀）。
        _ => b'S',
    };
    let _ = nix::unistd::write(verdict_w, &[verdict]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matches_by_name_and_numeric_id() {
        let content = "# comment\nalice:100000:65536\nbob:200000:65536\n1000:300000:1000\n";
        assert_eq!(
            parse_subid_ranges(content, Some("alice"), 1000),
            vec![(100000, 65536), (300000, 1000)]
        );
        assert_eq!(
            parse_subid_ranges(content, Some("bob"), 999),
            vec![(200000, 65536)]
        );
        assert_eq!(
            parse_subid_ranges(content, None, 1000),
            vec![(300000, 1000)]
        );
        assert!(parse_subid_ranges(content, Some("carol"), 42).is_empty());
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let content = "alice:100000\nalice:x:y\nalice:100000:0\nalice:100000:10\n";
        assert_eq!(
            parse_subid_ranges(content, Some("alice"), 1),
            vec![(100000, 10)]
        );
    }

    #[test]
    fn triples_chain_ranges_after_self() {
        assert_eq!(
            build_triples(1000, &[(100000, 65536), (300000, 1000)]),
            vec![(0, 1000, 1), (1, 100000, 65536), (65537, 300000, 1000)]
        );
    }
}
