//! 打鍵を PTY へのバイト列へ写す。
//!
//! グリッド描画とは無関係に完結する純粋関数の塊なので、独立したモジュールへ
//! 切り出してある。コピー・貼り付けの判定もここに含める — 通常のキー変換と
//! 同じ「`Keystroke` を見て何かを返す」という形の関数だから。

use gpui::{Keystroke, Modifiers};

/// コピーの打鍵か。
pub(super) fn is_copy_shortcut(keystroke: &Keystroke) -> bool {
    is_clipboard_shortcut(keystroke, "c", cfg!(target_os = "macos"))
}

/// 貼り付けの打鍵か。
pub(super) fn is_paste_shortcut(keystroke: &Keystroke) -> bool {
    is_clipboard_shortcut(keystroke, "v", cfg!(target_os = "macos"))
}

/// 端末のコピー・貼り付けの打鍵か。
///
/// **Ctrl+C をコピーにしてはいけない**。Ctrl+C は SIGINT であり、これを奪うと
/// 端末で走っているプロセスを止められなくなる。
///
/// macOS には ⌘ があるので ⌘C / ⌘V を使えば衝突しない。Windows/Linux には
/// その逃げ道が無いため、端末の慣習どおり Ctrl+Shift+C / Ctrl+Shift+V を使う。
/// アプリの他の場所 (エディタや入力欄) が `secondary-c` = Ctrl+C を使うのとは
/// 意図的に違えている。端末だけは Ctrl+C の意味が他と違うため。
///
/// `mac` を引数で受けるのは、どちらの作法も全プラットフォームの `cargo test` で
/// 検証できるようにするため。
fn is_clipboard_shortcut(keystroke: &Keystroke, key: &str, mac: bool) -> bool {
    if keystroke.key != key {
        return false;
    }
    let modifiers = keystroke.modifiers;
    if mac {
        modifiers.platform && !modifiers.control
    } else {
        modifiers.control && modifiers.shift && !modifiers.platform
    }
}

/// キー入力を PTY へ流すバイト列に変換する。
///
/// 返り値が `None` のキーは端末に送らない (アプリのショートカットへ譲る)。
pub(super) fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    // cmd (Windows では Win キー) と fn はアプリ側の割り当て。端末には渡さない。
    if modifiers.platform || modifiers.function {
        return None;
    }
    let key = keystroke.key.as_str();
    if defers_to_app(&modifiers, key) {
        return None;
    }

    let special: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" if modifiers.shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        _ => None,
    };
    if let Some(bytes) = special {
        return Some(bytes.to_vec());
    }

    if modifiers.control {
        return control_bytes(key);
    }

    // alt は meta として ESC 前置で送る。合成済みの文字ではなく素のキーを使う。
    if modifiers.alt {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(printable_text(keystroke)?.as_bytes());
        return Some(bytes);
    }

    Some(printable_text(keystroke)?.into_bytes())
}

/// 端末より先にアプリのショートカットへ譲る打鍵か。
///
/// macOS では ⌘ 修飾がアプリと端末を分ける役目を果たすので、端末が Ctrl 打鍵を
/// 全部受け取っても衝突しない。Windows/Linux にはその逃げ道が無く、アプリの
/// 割り当ても端末の制御文字もどちらも Ctrl を使う。
///
/// 基本は端末を優先する。Ctrl+C や Ctrl+R が効かない端末は端末として
/// 使いものにならないため。そのうえで「端末が制御文字として使わない打鍵」だけを
/// アプリへ譲る。制御文字は Ctrl+英字などで尽きており Ctrl+Shift+… に対応する
/// ものは無いので、パネル切り替え (Ctrl+Shift+E など) は安全に譲れる。
/// Ctrl+Tab も端末は使わず、アプリのタブ送りだけが使う。
///
/// この判定はプラットフォームで分けない。macOS でも Ctrl+Shift+… を端末が
/// 飲み込む理由は無く、譲った方が一貫する。
fn defers_to_app(modifiers: &Modifiers, key: &str) -> bool {
    if !modifiers.control {
        return false;
    }
    modifiers.shift || key == "tab"
}

/// 通常文字として送れる文字列。送れないキー (f1 など) は `None`。
fn printable_text(keystroke: &Keystroke) -> Option<String> {
    if keystroke.key == "space" {
        return Some(" ".to_string());
    }
    // key_char は option-s → "ß" のような合成結果を含む。あればそちらを優先する。
    if let Some(text) = keystroke.key_char.as_deref()
        && !text.is_empty()
        && !text.chars().any(|c| c.is_control())
    {
        return Some(text.to_string());
    }
    let mut chars = keystroke.key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}

/// Ctrl 併用時の制御文字。
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    if key == "space" {
        return Some(vec![0]);
    }
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let byte = match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(vec![byte])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.to_string(),
            key_char: (name.chars().count() == 1).then(|| name.to_string()),
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    // -- キー変換 --

    #[test]
    fn 通常文字は_utf8_バイトになる() {
        assert_eq!(
            keystroke_to_bytes(&key("a", Modifiers::default())),
            Some(b"a".to_vec())
        );
        let mut ime = key("a", Modifiers::default());
        ime.key_char = Some("あ".to_string());
        assert_eq!(keystroke_to_bytes(&ime), Some("あ".as_bytes().to_vec()));
    }

    #[test]
    fn 特殊キーがエスケープシーケンスになる() {
        let table = [
            ("enter", b"\r".to_vec()),
            ("tab", b"\t".to_vec()),
            ("backspace", b"\x7f".to_vec()),
            ("escape", b"\x1b".to_vec()),
            ("up", b"\x1b[A".to_vec()),
            ("down", b"\x1b[B".to_vec()),
            ("right", b"\x1b[C".to_vec()),
            ("left", b"\x1b[D".to_vec()),
            ("home", b"\x1b[H".to_vec()),
            ("end", b"\x1b[F".to_vec()),
            ("pageup", b"\x1b[5~".to_vec()),
            ("pagedown", b"\x1b[6~".to_vec()),
            ("delete", b"\x1b[3~".to_vec()),
        ];
        for (name, expected) in table {
            assert_eq!(
                keystroke_to_bytes(&key(name, Modifiers::default())),
                Some(expected),
                "{name} の変換"
            );
        }
    }

    #[test]
    fn ctrl_併用で制御文字になる() {
        assert_eq!(keystroke_to_bytes(&key("a", ctrl())), Some(vec![0x01]));
        assert_eq!(keystroke_to_bytes(&key("c", ctrl())), Some(vec![0x03]));
        assert_eq!(keystroke_to_bytes(&key("z", ctrl())), Some(vec![0x1a]));
        assert_eq!(keystroke_to_bytes(&key("space", ctrl())), Some(vec![0x00]));
    }

    #[test]
    fn cmd_併用は端末に送らない() {
        assert_eq!(keystroke_to_bytes(&key("c", cmd())), None);
        assert_eq!(keystroke_to_bytes(&key("f1", Modifiers::default())), None);
    }

    #[test]
    fn shift_tab_は逆タブ_alt_は_esc_前置() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            keystroke_to_bytes(&key("tab", shift)),
            Some(b"\x1b[Z".to_vec())
        );
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(keystroke_to_bytes(&key("b", alt)), Some(b"\x1bb".to_vec()));
    }

    fn cmd() -> Modifiers {
        Modifiers {
            platform: true,
            ..Modifiers::default()
        }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn macos_では_cmd_c_がコピーで_ctrl_c_は_sigint_のまま() {
        assert!(is_clipboard_shortcut(&key("c", cmd()), "c", true));
        // Ctrl+C は SIGINT。誤ってコピー扱いすると端末でプロセスを止められなくなる。
        assert!(!is_clipboard_shortcut(&key("c", ctrl()), "c", true));
    }

    /// Windows/Linux には ⌘ が無い。Ctrl+C を奪えないので Ctrl+Shift+C を使う。
    #[test]
    fn windows_では_ctrl_shift_c_がコピーで_ctrl_c_は_sigint_のまま() {
        assert!(is_clipboard_shortcut(&key("c", ctrl_shift()), "c", false));
        assert!(!is_clipboard_shortcut(&key("c", ctrl()), "c", false));
        // ⌘ は Windows では Win キー。コピーではない。
        assert!(!is_clipboard_shortcut(&key("c", cmd()), "c", false));
    }

    #[test]
    fn 貼り付けもコピーと同じ作法で判定する() {
        assert!(is_clipboard_shortcut(&key("v", cmd()), "v", true));
        assert!(is_clipboard_shortcut(&key("v", ctrl_shift()), "v", false));
        assert!(!is_clipboard_shortcut(
            &key("v", Modifiers::default()),
            "v",
            true
        ));
        // 別のキーには反応しない。
        assert!(!is_clipboard_shortcut(&key("x", ctrl_shift()), "v", false));
    }

    /// Ctrl+Shift+… に対応する制御文字は無い。端末が飲み込むと、Windows では
    /// 端末に居るあいだパネルを切り替えられなくなる。
    #[test]
    fn ctrl_shift_併用はアプリへ譲る() {
        assert_eq!(keystroke_to_bytes(&key("e", ctrl_shift())), None);
        assert_eq!(keystroke_to_bytes(&key("tab", ctrl())), None);
        assert_eq!(keystroke_to_bytes(&key("tab", ctrl_shift())), None);
        // 素の Ctrl+英字は端末のもの。譲ってはいけない。
        assert_eq!(keystroke_to_bytes(&key("e", ctrl())), Some(vec![0x05]));
    }
}
