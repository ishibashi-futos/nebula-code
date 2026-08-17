//! 埋め込みアセット。
//!
//! アイコンはバイナリに埋め込む。外部ファイルを読みに行くと、冷間起動時に
//! ディスク I/O が初回フレームの前に挟まるうえ、配布物の構成も壊れやすい。

use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// SVG アイコン。名前は拡張子なしで参照する (`icons/search`)。
static ICONS: &[(&str, &str)] = &[
    ("branch", include_str!("../assets/icons/branch.svg")),
    ("check", include_str!("../assets/icons/check.svg")),
    (
        "chevron-down",
        include_str!("../assets/icons/chevron-down.svg"),
    ),
    (
        "chevron-right",
        include_str!("../assets/icons/chevron-right.svg"),
    ),
    ("close", include_str!("../assets/icons/close.svg")),
    ("copy", include_str!("../assets/icons/copy.svg")),
    ("edit", include_str!("../assets/icons/edit.svg")),
    ("error", include_str!("../assets/icons/error.svg")),
    ("file", include_str!("../assets/icons/file.svg")),
    ("files", include_str!("../assets/icons/files.svg")),
    ("folder", include_str!("../assets/icons/folder.svg")),
    ("git", include_str!("../assets/icons/git.svg")),
    ("panel", include_str!("../assets/icons/panel.svg")),
    ("plus", include_str!("../assets/icons/plus.svg")),
    ("problems", include_str!("../assets/icons/problems.svg")),
    ("refresh", include_str!("../assets/icons/refresh.svg")),
    ("search", include_str!("../assets/icons/search.svg")),
    ("send", include_str!("../assets/icons/send.svg")),
    ("settings", include_str!("../assets/icons/settings.svg")),
    ("sparkles", include_str!("../assets/icons/sparkles.svg")),
    ("split", include_str!("../assets/icons/split.svg")),
    ("stop", include_str!("../assets/icons/stop.svg")),
    ("terminal", include_str!("../assets/icons/terminal.svg")),
    ("trash", include_str!("../assets/icons/trash.svg")),
    ("undo", include_str!("../assets/icons/undo.svg")),
    ("warning", include_str!("../assets/icons/warning.svg")),
];

pub struct NebulaAssets;

impl AssetSource for NebulaAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let name = path
            .strip_prefix("icons/")
            .map(|n| n.strip_suffix(".svg").unwrap_or(n));
        Ok(name
            .and_then(|name| ICONS.iter().find(|(id, _)| *id == name))
            .map(|(_, svg)| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(id, _)| SharedString::from(format!("icons/{id}.svg")))
            .collect())
    }
}

/// アイコン名。存在しない名前をコンパイル時に弾くため、文字列ではなく列挙で持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Branch,
    Check,
    ChevronDown,
    ChevronRight,
    Close,
    Copy,
    Edit,
    Error,
    File,
    Files,
    Folder,
    Git,
    Panel,
    Plus,
    Problems,
    Refresh,
    Search,
    Send,
    Settings,
    Sparkles,
    Split,
    Stop,
    Terminal,
    Trash,
    Undo,
    Warning,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Icon::Branch => "icons/branch.svg",
            Icon::Check => "icons/check.svg",
            Icon::ChevronDown => "icons/chevron-down.svg",
            Icon::ChevronRight => "icons/chevron-right.svg",
            Icon::Close => "icons/close.svg",
            Icon::Copy => "icons/copy.svg",
            Icon::Edit => "icons/edit.svg",
            Icon::Error => "icons/error.svg",
            Icon::File => "icons/file.svg",
            Icon::Files => "icons/files.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::Git => "icons/git.svg",
            Icon::Panel => "icons/panel.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::Problems => "icons/problems.svg",
            Icon::Refresh => "icons/refresh.svg",
            Icon::Search => "icons/search.svg",
            Icon::Send => "icons/send.svg",
            Icon::Settings => "icons/settings.svg",
            Icon::Sparkles => "icons/sparkles.svg",
            Icon::Split => "icons/split.svg",
            Icon::Stop => "icons/stop.svg",
            Icon::Terminal => "icons/terminal.svg",
            Icon::Trash => "icons/trash.svg",
            Icon::Undo => "icons/undo.svg",
            Icon::Warning => "icons/warning.svg",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全アイコンが埋め込まれている() {
        let assets = NebulaAssets;
        let all = [
            Icon::Branch,
            Icon::Check,
            Icon::ChevronDown,
            Icon::ChevronRight,
            Icon::Close,
            Icon::Copy,
            Icon::Edit,
            Icon::Error,
            Icon::File,
            Icon::Files,
            Icon::Folder,
            Icon::Git,
            Icon::Panel,
            Icon::Plus,
            Icon::Problems,
            Icon::Refresh,
            Icon::Search,
            Icon::Send,
            Icon::Settings,
            Icon::Sparkles,
            Icon::Split,
            Icon::Stop,
            Icon::Terminal,
            Icon::Trash,
            Icon::Undo,
            Icon::Warning,
        ];
        assert_eq!(all.len(), ICONS.len());
        for icon in all {
            assert!(
                assets.load(icon.path()).unwrap().is_some(),
                "{} が読み込めない",
                icon.path()
            );
        }
    }

    #[test]
    fn 存在しないアセットは_none() {
        assert!(NebulaAssets.load("icons/nope.svg").unwrap().is_none());
    }
}
