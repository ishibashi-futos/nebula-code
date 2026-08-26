//! 新規作成・リネーム・複製の名前決め。
//!
//! 入力欄に打たれた名前の検証と、複製先ファイル名の生成という、UI 状態を
//! 一切参照しない純粋なパス処理だけを集めた塊。

use std::path::{Path, PathBuf};

/// 入力された名前を検証する。問題があれば利用者に見せる文言を返す。
pub(super) fn validate_name(name: &str) -> Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名前を入力してください".into());
    }
    if trimmed.contains('/') {
        return Err("名前に / は使えません".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("その名前は使えません".into());
    }
    Ok(trimmed)
}

/// 複製先のパス。`foo.rs` → `foo copy.rs`。
///
/// 拡張子の前に付けるのは、複製しても言語判定とアイコンが変わらないようにするため。
pub(super) fn duplicate_target(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(""));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // 先頭のドットは拡張子ではなく隠しファイルの印なので、区切りとして数えない。
    let dot = name
        .char_indices()
        .skip(1)
        .filter(|(_, c)| *c == '.')
        .map(|(i, _)| i)
        .last();
    let renamed = match dot {
        Some(i) => format!("{} copy{}", &name[..i], &name[i..]),
        None => format!("{name} copy"),
    };
    parent.join(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 名前の検証() {
        assert_eq!(validate_name("  main.rs "), Ok("main.rs"));
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn 複製先は拡張子の前に付ける() {
        assert_eq!(
            duplicate_target(Path::new("/w/src/main.rs")),
            PathBuf::from("/w/src/main copy.rs")
        );
        assert_eq!(
            duplicate_target(Path::new("/w/src")),
            PathBuf::from("/w/src copy")
        );
        assert_eq!(
            duplicate_target(Path::new("/w/.gitignore")),
            PathBuf::from("/w/.gitignore copy"),
            "先頭のドットは拡張子ではない"
        );
        assert_eq!(
            duplicate_target(Path::new("/w/a.tar.gz")),
            PathBuf::from("/w/a.tar copy.gz")
        );
    }
}
