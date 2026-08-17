//! セッションの保存と復元。
//!
//! 前回どのフォルダを開いていたかを覚えておき、次の起動でそのまま作業を再開できるようにする。
//!
//! 開いていたファイルまでは復元しない。復元するとバッファを開く要求が起動直後に
//! 何本も走り、最初のフレームの後とはいえ体感の立ち上がりが鈍る。VS Code のように
//! 「フォルダは覚える、タブは覚えない」を既定とする。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// 開いていたワークスペースのルート。並び順は表示順。
    #[serde(default)]
    pub workspaces: Vec<PathBuf>,
    /// 選択されていたワークスペース。
    #[serde(default)]
    pub active: Option<PathBuf>,
    /// サイドバーの幅 (ピクセル)。
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// 下部パネルの高さ (ピクセル)。
    #[serde(default)]
    pub panel_height: Option<f32>,
    /// サイドバーを開いていたか。
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
}

fn default_true() -> bool {
    true
}

/// `Default` は手書きにする。
///
/// derive すると `sidebar_visible` が `false` になり、
/// 「ファイルが無いとき」と「空の JSON を読んだとき」で初期状態が食い違う。
impl Default for Session {
    fn default() -> Self {
        Self {
            workspaces: Vec::new(),
            active: None,
            sidebar_width: None,
            panel_height: None,
            sidebar_visible: true,
        }
    }
}

/// セッションファイルの位置。
///
/// macOS はアプリの設定を `~/Library/Application Support` に置く決まりなので、
/// XDG の `~/.config` は使わない。実際、この環境の `~/.config` は書き込み権限が無く
/// (`d--x--x--x`)、そちらに書こうとすると黙って失敗していた。
/// `XDG_CONFIG_HOME` が明示されている場合だけは、利用者の指定として尊重する。
fn session_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("session.json"))
}

fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("nebula"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Application Support").join("Nebula"))
    } else {
        Some(home.join(".config").join("nebula"))
    }
}

impl Session {
    /// 読み込む。壊れていたり無かったりすれば既定値を返す。
    ///
    /// 起動を止める理由にはならないので、失敗は握りつぶす。
    pub fn load() -> Self {
        session_path()
            .map(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    /// 保存する。作業内容ではないので、失敗しても利用者の操作は止めない。
    /// ただし黙って捨てるとセッションが復元されない理由が分からなくなるため、
    /// 計測モードのときだけ標準エラーに理由を出す。
    pub fn save(&self) {
        let Some(path) = session_path() else {
            return;
        };
        if let Err(e) = self.try_save_to(&path) {
            if crate::trace_startup() {
                eprintln!("nebula: セッションを保存できません ({}): {e}", path.display());
            }
        }
    }

    /// 経路を明示して読み込む。テストから使う。
    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 経路を明示して保存する。
    ///
    /// 書き込みは一時ファイル経由にする。保存中に落ちると壊れた JSON が残り、
    /// 次の起動で既定値に戻ってしまう。
    fn try_save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path)
    }

    /// 存在しなくなったフォルダを落とす。
    ///
    /// 消えたフォルダを開こうとするとエラー通知が出て、起動のたびに邪魔になる。
    pub fn prune_missing(&mut self) {
        self.workspaces.retain(|path| path.is_dir());
        if let Some(active) = &self.active
            && !self.workspaces.contains(active)
        {
            self.active = self.workspaces.first().cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 既定値ではサイドバーが開いている() {
        assert!(Session::default().sidebar_visible);
    }

    #[test]
    fn ファイル無しと空_json_で初期状態が一致する() {
        let from_empty: Session = serde_json::from_str("{}").unwrap();
        assert_eq!(from_empty, Session::default());
    }

    #[test]
    fn 空の_json_からでも読める() {
        let session: Session = serde_json::from_str("{}").unwrap();
        assert!(session.workspaces.is_empty());
        assert!(session.sidebar_visible, "既定値が効いている");
    }

    #[test]
    fn 往復できる() {
        let session = Session {
            workspaces: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            active: Some(PathBuf::from("/b")),
            sidebar_width: Some(300.0),
            panel_height: Some(200.0),
            sidebar_visible: false,
        };
        let text = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&text).unwrap(), session);
    }

    #[test]
    fn ファイルへ書いて読み戻せる() {
        let dir = std::env::temp_dir().join(format!("nebula-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("session.json");

        let session = Session {
            workspaces: vec![PathBuf::from("/w1"), PathBuf::from("/w2")],
            active: Some(PathBuf::from("/w2")),
            sidebar_width: Some(321.0),
            panel_height: Some(210.0),
            sidebar_visible: false,
        };
        session.try_save_to(&path).expect("保存できる場所のはず");
        assert!(path.exists(), "保存先が作られていない: {}", path.display());
        assert_eq!(Session::load_from(&path), session);

        // 一時ファイルが残らないこと。
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 保存先は_macos_の作法に従う() {
        // 環境変数を書き換えずに判定だけ確かめる。
        let dir = config_dir().expect("HOME があれば必ず決まる");
        if std::env::var_os("XDG_CONFIG_HOME").is_some() {
            assert!(dir.ends_with("nebula"));
        } else if cfg!(target_os = "macos") {
            assert!(
                dir.ends_with("Library/Application Support/Nebula"),
                "macOS では ~/Library/Application Support に置く: {}",
                dir.display()
            );
        } else {
            assert!(dir.ends_with(".config/nebula"));
        }
    }

    #[test]
    fn 書き込めない場所への保存は失敗を返す() {
        let session = Session::default();
        // 存在しない上に作れないルート直下。
        let result = session.try_save_to(std::path::Path::new("/nebula-no-such-root/session.json"));
        assert!(result.is_err(), "失敗が握りつぶされている");
    }

    #[test]
    fn 壊れた_json_は既定値になる() {
        let dir = std::env::temp_dir().join(format!("nebula-session-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, "{ こわれている").unwrap();
        assert_eq!(Session::load_from(&path), Session::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 存在しないフォルダは落とされる() {
        let existing = std::env::temp_dir();
        let mut session = Session {
            workspaces: vec![existing.clone(), PathBuf::from("/nebula/no/such/dir")],
            active: Some(PathBuf::from("/nebula/no/such/dir")),
            ..Session::default()
        };
        session.prune_missing();
        assert_eq!(session.workspaces, vec![existing.clone()]);
        assert_eq!(session.active, Some(existing), "選択も生き残りへ移る");
    }

    #[test]
    fn 全部消えたら選択も_none() {
        let mut session = Session {
            workspaces: vec![PathBuf::from("/nebula/no/such/dir")],
            active: Some(PathBuf::from("/nebula/no/such/dir")),
            ..Session::default()
        };
        session.prune_missing();
        assert!(session.workspaces.is_empty());
        assert_eq!(session.active, None);
    }
}
