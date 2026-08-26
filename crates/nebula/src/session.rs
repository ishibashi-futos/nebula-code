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
    /// エクスプローラーで隠しファイルを表示していたか。
    ///
    /// 既定は非表示なので、`bool` の既定値 (`false`) をそのまま使ってよい。
    /// `sidebar_visible` と違って専用の default 関数が要らない。
    #[serde(default)]
    pub explorer_show_hidden: bool,
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
            explorer_show_hidden: false,
        }
    }
}

/// セッションファイルの位置。
///
/// macOS はアプリの設定を `~/Library/Application Support` に置く決まりなので、
/// XDG の `~/.config` は使わない。実際、この環境の `~/.config` は書き込み権限が無く
/// (`d--x--x--x`)、そちらに書こうとすると黙って失敗していた。
/// `XDG_CONFIG_HOME` が明示されている場合だけは、利用者の指定として尊重する。
///
/// Windows は既定で `HOME` を設定しない。`HOME` 前提のままだと `config_dir()` が
/// 常に `None` を返し、セッション (開いていたフォルダやサイドバー幅など) が
/// Windows では一切保存/復元されない。Windows の作法に従って `%APPDATA%\Nebula` を使い、
/// `APPDATA` が無い場合だけ `%USERPROFILE%\AppData\Roaming` から組み立てる。
fn session_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("session.json"))
}

/// OS ごとの設定ディレクトリの流儀。
///
/// `cfg!(target_os = "windows")` はコンパイル時に確定してしまうので、macOS 上でビルドした
/// テストからは Windows 分岐を一度も通せない。判定結果を値として受け渡せるようにしておき、
/// `config_dir_from` をテストから任意の OS になりすまして叩けるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetOs {
    Macos,
    Windows,
    Other,
}

impl TargetOs {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// 環境変数の値から設定ディレクトリを決める純粋関数。
///
/// 環境変数を直接読まないことで、macOS 上の `cargo test` からも Windows / それ以外の
/// 分岐を検証できる。実際の環境変数の読み出しは呼び出し元の `config_dir()` でだけ行う。
fn config_dir_from(
    os: TargetOs,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    appdata: Option<PathBuf>,
    userprofile: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        return Some(xdg.join("nebula"));
    }
    match os {
        // HOME は Windows の作法ではないので、ここでは一切参照しない。
        TargetOs::Windows => appdata
            .or_else(|| userprofile.map(|dir| dir.join("AppData").join("Roaming")))
            .map(|dir| dir.join("Nebula")),
        TargetOs::Macos => Some(
            home?
                .join("Library")
                .join("Application Support")
                .join("Nebula"),
        ),
        TargetOs::Other => Some(home?.join(".config").join("nebula")),
    }
}

fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        TargetOs::current(),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("APPDATA").map(PathBuf::from),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
    )
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
    fn 既定では隠しファイルを表示しない() {
        assert!(!Session::default().explorer_show_hidden);
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
            explorer_show_hidden: true,
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
            explorer_show_hidden: true,
        };
        session.try_save_to(&path).expect("保存できる場所のはず");
        assert!(path.exists(), "保存先が作られていない: {}", path.display());
        assert_eq!(Session::load_from(&path), session);

        // 一時ファイルが残らないこと。
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 保存先は各プラットフォームの作法に従う() {
        // 実行環境の HOME/APPDATA には左右されない。config_dir_from に値を渡して確かめる。

        // macOS: ~/Library/Application Support/Nebula
        let macos = config_dir_from(
            TargetOs::Macos,
            None,
            Some(PathBuf::from("/Users/someone")),
            None,
            None,
        )
        .expect("HOME があれば必ず決まる");
        assert!(
            macos.ends_with("Library/Application Support/Nebula"),
            "macOS では ~/Library/Application Support に置く: {}",
            macos.display()
        );

        // Windows: %APPDATA%\Nebula
        let windows = config_dir_from(
            TargetOs::Windows,
            None,
            None,
            Some(PathBuf::from("C:/Users/someone/AppData/Roaming")),
            None,
        )
        .expect("APPDATA があれば必ず決まる");
        assert!(
            windows.ends_with("Nebula"),
            "Windows では %APPDATA% 配下に置く: {}",
            windows.display()
        );

        // Windows: APPDATA が無ければ %USERPROFILE%\AppData\Roaming から組み立てる
        let windows_fallback = config_dir_from(
            TargetOs::Windows,
            None,
            None,
            None,
            Some(PathBuf::from("C:/Users/someone")),
        )
        .expect("USERPROFILE があれば必ず決まる");
        assert!(
            windows_fallback.ends_with("AppData/Roaming/Nebula"),
            "APPDATA が無ければ USERPROFILE から組み立てる: {}",
            windows_fallback.display()
        );

        // Windows: 両方あれば APPDATA を優先する
        let windows_both = config_dir_from(
            TargetOs::Windows,
            None,
            None,
            Some(PathBuf::from("D:/CustomAppData")),
            Some(PathBuf::from("C:/Users/someone")),
        )
        .expect("APPDATA があれば必ず決まる");
        assert!(
            windows_both.ends_with("CustomAppData/Nebula"),
            "APPDATA と USERPROFILE の両方があれば APPDATA を優先する: {}",
            windows_both.display()
        );

        // Windows: どちらも無ければ諦める。HOME があっても使わない。
        assert_eq!(
            config_dir_from(TargetOs::Windows, None, Some(PathBuf::from("/x")), None, None),
            None,
        );

        // それ以外 (Linux 等): ~/.config/nebula
        let other = config_dir_from(
            TargetOs::Other,
            None,
            Some(PathBuf::from("/home/someone")),
            None,
            None,
        )
        .expect("HOME があれば必ず決まる");
        assert!(other.ends_with(".config/nebula"));

        // macOS / それ以外: HOME が無ければ諦める (既存の挙動を変えない)
        assert_eq!(config_dir_from(TargetOs::Macos, None, None, None, None), None);
        assert_eq!(config_dir_from(TargetOs::Other, None, None, None, None), None);

        // XDG_CONFIG_HOME が明示されていれば OS を問わず最優先する
        let xdg = config_dir_from(
            TargetOs::Windows,
            Some(PathBuf::from("/custom/config")),
            None,
            None,
            None,
        )
        .expect("XDG_CONFIG_HOME があれば必ず決まる");
        assert!(xdg.ends_with("nebula"));
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
