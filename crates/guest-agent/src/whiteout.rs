//! OCI layer 的 whiteout 檔名解析（純函式，跨平台可測）。
//!
//! 規格（OCI image-spec layer.md）：
//! - `.wh.<name>`：刪除下層的 `<name>`（檔案或整棵目錄）。
//! - `.wh..wh..opq`：opaque whiteout——清空所在目錄來自下層的內容，保留目錄本身。
//!
//! whiteout entry 本身不落地。

/// whiteout 檔名前綴。
pub const WHITEOUT_PREFIX: &str = ".wh.";
/// opaque whiteout 的完整檔名。
pub const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// 解析結果：對 rootfs 應採取的動作（路徑皆為以 `/` 分隔的相對路徑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhiteoutAction {
    /// 清空 `dir` 目錄的既有內容（保留目錄本身）；`dir` 為空字串表示 rootfs 根。
    Opaque { dir: String },
    /// 刪除 `path` 對應的檔案或整棵目錄。
    Remove { path: String },
}

/// 判斷一個 tar entry 路徑是否為 whiteout 標記。
///
/// 回傳 `None` 表示這是一般 entry，應正常解開。
/// 輸入為 tar 內的路徑（以 `/` 分隔；允許 `./` 前綴與尾端 `/`）。
pub fn parse_whiteout(entry_path: &str) -> Option<WhiteoutAction> {
    let trimmed = entry_path.trim_end_matches('/');
    let (dir, name) = match trimmed.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", trimmed),
    };
    if name == OPAQUE_WHITEOUT {
        return Some(WhiteoutAction::Opaque {
            dir: dir.to_string(),
        });
    }
    if let Some(stripped) = name.strip_prefix(WHITEOUT_PREFIX) {
        // `.wh.` 後面必須有目標名稱；空目標視為一般 entry（避免誤刪整個目錄）
        if stripped.is_empty() {
            return None;
        }
        let path = if dir.is_empty() {
            stripped.to_string()
        } else {
            format!("{dir}/{stripped}")
        };
        return Some(WhiteoutAction::Remove { path });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_entries_are_not_whiteouts() {
        assert_eq!(parse_whiteout("etc/passwd"), None);
        assert_eq!(parse_whiteout("usr/bin/"), None);
        assert_eq!(parse_whiteout("a/b/c.txt"), None);
        // `.wh` 不足前綴、或前綴出現在中段，都不是 whiteout
        assert_eq!(parse_whiteout("a/.whx"), None);
        assert_eq!(parse_whiteout("a/x.wh.y"), None);
    }

    #[test]
    fn remove_whiteout_at_root() {
        assert_eq!(
            parse_whiteout(".wh.foo"),
            Some(WhiteoutAction::Remove { path: "foo".into() })
        );
    }

    #[test]
    fn remove_whiteout_in_subdir() {
        assert_eq!(
            parse_whiteout("var/lib/.wh.cache"),
            Some(WhiteoutAction::Remove {
                path: "var/lib/cache".into()
            })
        );
    }

    #[test]
    fn opaque_whiteout() {
        assert_eq!(
            parse_whiteout("etc/conf.d/.wh..wh..opq"),
            Some(WhiteoutAction::Opaque {
                dir: "etc/conf.d".into()
            })
        );
        assert_eq!(
            parse_whiteout(".wh..wh..opq"),
            Some(WhiteoutAction::Opaque { dir: "".into() })
        );
    }

    #[test]
    fn trailing_slash_and_dot_prefix() {
        assert_eq!(
            parse_whiteout("./a/.wh.b"),
            Some(WhiteoutAction::Remove {
                path: "./a/b".into()
            })
        );
        assert_eq!(
            parse_whiteout("a/b/.wh..wh..opq/"),
            Some(WhiteoutAction::Opaque { dir: "a/b".into() })
        );
    }

    #[test]
    fn bare_prefix_is_not_a_whiteout() {
        // `.wh.` 沒有目標名稱 → 不是 whiteout（避免空路徑誤刪）
        assert_eq!(parse_whiteout("a/.wh."), None);
        assert_eq!(parse_whiteout(".wh."), None);
    }
}
