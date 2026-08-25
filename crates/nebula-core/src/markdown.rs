//! Markdown 編集支援。
//!
//! 箇条書き・チェックリスト・番号付きリスト・引用を改行時に自動継続するための
//! 純粋関数を提供する。行の文字列だけを見て判定し、`Rope` にも GUI にも触れない。
//! こうしておくと GPUI を起動しなくてもロジックの正しさをテストできる。

/// 改行時、リストや引用の行に対してどう振る舞うべきかを表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListContinuation {
    /// 新しい行の先頭にこの文字列 (インデント込みの新しいマーカー) を入れて続ける。
    Continue(String),
    /// マーカーだけで中身が空の行だったので、リストから抜ける合図。
    /// 呼び出し側は改行を挿入せず、現在行のマーカーを消して空行にする。
    Terminate,
}

/// 行の内容から、改行時のリスト継続を判定する。
///
/// 認識するのは次の行頭パターン。いずれも先頭の空白 (インデント) はそのまま
/// 引き継ぐので、ネストしたリストの入れ子も保たれる。
///
/// - 箇条書き `- ` / `* ` / `+ ` (使われた記号をそのまま次の行にも使う)
/// - チェックリスト `- [ ] ` / `- [x] ` (チェック状態は引き継がず、常に未チェックで続ける)
/// - 番号付きリスト `1. ` / `2) ` (番号を 1 つ増やして続ける)
/// - 引用 `> `
///
/// マーカーの後ろに中身が無い (空白しか無い) 行は、そこでリストを打ち切りたいという
/// 合図なので [`ListContinuation::Terminate`] を返す。これが無いと、空の項目で
/// 改行するたびにマーカーが増え続け、リストから抜けられなくなる。
///
/// 該当する行頭パターンが無い行 (見出しや普通の文章、水平線 `---` など) は `None`。
pub fn list_continuation(line: &str) -> Option<ListContinuation> {
    let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    let (indent, rest) = line.split_at(indent_len);

    let (consumed, next_marker) = parse_checklist(rest)
        .or_else(|| parse_bullet(rest))
        .or_else(|| parse_ordered(rest))
        .or_else(|| parse_blockquote(rest))?;

    // マーカーの後ろが空白だけなら、その項目には中身が無い。
    if rest[consumed..].trim().is_empty() {
        Some(ListContinuation::Terminate)
    } else {
        Some(ListContinuation::Continue(format!("{indent}{next_marker}")))
    }
}

/// チェックリスト `- [ ] ` / `- [x] ` / `- [X] `。
///
/// 箇条書きより先に判定する必要がある。`- [ ] ` は `- ` にもマッチしてしまうため、
/// 呼び出し順で優先度を表す。
fn parse_checklist(rest: &str) -> Option<(usize, String)> {
    let bytes = rest.as_bytes();
    if bytes.len() < 6 || bytes[0] != b'-' || bytes[1] != b' ' {
        return None;
    }
    if bytes[2] != b'[' || bytes[4] != b']' || bytes[5] != b' ' {
        return None;
    }
    if !matches!(bytes[3], b' ' | b'x' | b'X') {
        return None;
    }
    // チェック済みかどうかによらず、継続後は常に未チェックにする。
    Some((6, "- [ ] ".to_string()))
}

/// 箇条書き `- ` / `* ` / `+ `。使われた記号をそのまま引き継ぐ。
fn parse_bullet(rest: &str) -> Option<(usize, String)> {
    let bytes = rest.as_bytes();
    if bytes.len() < 2 || !matches!(bytes[0], b'-' | b'*' | b'+') || bytes[1] != b' ' {
        return None;
    }
    // bytes[0] は '-' '*' '+' のいずれかで 1 バイトの ASCII と確定しているので、
    // u8 -> char の変換がそのまま元の文字になる。
    Some((2, format!("{} ", bytes[0] as char)))
}

/// 番号付きリスト `1. ` / `2) `。番号を 1 つ増やして継続する。
///
/// 桁上がり (`9.` の次は `10.`) は単純な数値の加算なので、桁数を揃える必要はない。
fn parse_ordered(rest: &str) -> Option<(usize, String)> {
    let digits_len = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_len == 0 {
        return None;
    }
    let num: u64 = rest[..digits_len].parse().ok()?;
    let mut tail = rest[digits_len..].chars();
    let sep = tail.next()?;
    if sep != '.' && sep != ')' {
        return None;
    }
    if tail.next() != Some(' ') {
        return None;
    }
    // 区切り文字 (`.` か `)`) と直後の半角スペースはどちらも 1 バイトの ASCII。
    let consumed = digits_len + 2;
    // 現実的な桁数のリストでしか使わないが、万一の桁溢れでも panic はさせない。
    let next = num.saturating_add(1);
    Some((consumed, format!("{next}{sep} ")))
}

/// 引用 `> `。`>` 単体の行や、スペースを省いた `>text` (遅延継続) も同じ扱いにする。
fn parse_blockquote(rest: &str) -> Option<(usize, String)> {
    if !rest.starts_with('>') {
        return None;
    }
    let consumed = if rest[1..].starts_with(' ') { 2 } else { 1 };
    Some((consumed, "> ".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ハイフンの箇条書きが継続する() {
        assert_eq!(
            list_continuation("- 項目"),
            Some(ListContinuation::Continue("- ".to_string()))
        );
    }

    #[test]
    fn アスタリスクの箇条書きが継続する() {
        assert_eq!(
            list_continuation("* 項目"),
            Some(ListContinuation::Continue("* ".to_string()))
        );
    }

    #[test]
    fn プラスの箇条書きが継続する() {
        assert_eq!(
            list_continuation("+ 項目"),
            Some(ListContinuation::Continue("+ ".to_string()))
        );
    }

    #[test]
    fn ネストした箇条書きは先頭の空白を保持する() {
        // インデントが無いと、ネストを維持したいのに親の階層まで戻ってしまう。
        assert_eq!(
            list_continuation("  - 子項目"),
            Some(ListContinuation::Continue("  - ".to_string()))
        );
    }

    #[test]
    fn タブでネストした箇条書きも先頭の空白を保持する() {
        assert_eq!(
            list_continuation("\t- 子項目"),
            Some(ListContinuation::Continue("\t- ".to_string()))
        );
    }

    #[test]
    fn 番号付きリストは番号を1つ増やして継続する() {
        assert_eq!(
            list_continuation("1. 項目"),
            Some(ListContinuation::Continue("2. ".to_string()))
        );
    }

    #[test]
    fn 番号付きリストは9から10へ桁上がりする() {
        // 単純な文字列インクリメントだと "9." -> "10." のような桁上がりで壊れやすい。
        // 数値として足し算していることを確認する。
        assert_eq!(
            list_continuation("9. 項目"),
            Some(ListContinuation::Continue("10. ".to_string()))
        );
    }

    #[test]
    fn 括弧区切りの番号付きリストも継続する() {
        assert_eq!(
            list_continuation("2) 項目"),
            Some(ListContinuation::Continue("3) ".to_string()))
        );
    }

    #[test]
    fn 未チェックのチェックリストが継続する() {
        assert_eq!(
            list_continuation("- [ ] 項目"),
            Some(ListContinuation::Continue("- [ ] ".to_string()))
        );
    }

    #[test]
    fn チェック済みのチェックリストは未チェックで継続する() {
        // チェック状態を引き継ぐと、次の項目が最初からチェック済みになってしまい
        // 「次にやること」を書くリストとして機能しない。
        assert_eq!(
            list_continuation("- [x] 完了した項目"),
            Some(ListContinuation::Continue("- [ ] ".to_string()))
        );
    }

    #[test]
    fn 大文字xのチェック済みも未チェックで継続する() {
        assert_eq!(
            list_continuation("- [X] 完了した項目"),
            Some(ListContinuation::Continue("- [ ] ".to_string()))
        );
    }

    #[test]
    fn 引用が継続する() {
        assert_eq!(
            list_continuation("> 引用文"),
            Some(ListContinuation::Continue("> ".to_string()))
        );
    }

    #[test]
    fn 空の箇条書きは打ち切ってマーカーを消す() {
        // VS Code や Typora と同じ挙動: マーカーだけの行で改行すると、リストから
        // 抜けて空行になる。継続してしまうとマーカーが無限に増えて抜けられない。
        assert_eq!(list_continuation("- "), Some(ListContinuation::Terminate));
    }

    #[test]
    fn 空のチェックリストは打ち切ってマーカーを消す() {
        assert_eq!(
            list_continuation("- [ ] "),
            Some(ListContinuation::Terminate)
        );
    }

    #[test]
    fn 空の番号付きリストは打ち切ってマーカーを消す() {
        assert_eq!(list_continuation("1. "), Some(ListContinuation::Terminate));
    }

    #[test]
    fn 空の引用は打ち切ってマーカーを消す() {
        assert_eq!(list_continuation("> "), Some(ListContinuation::Terminate));
    }

    #[test]
    fn 空白だけの箇条書きも打ち切りとみなす() {
        // マーカーの後ろが半角スペースの連続なだけで、trim すれば空になる場合も
        // 「中身が無い」と判定する。
        assert_eq!(
            list_continuation("-    "),
            Some(ListContinuation::Terminate)
        );
    }

    #[test]
    fn リストでない普通の行はnone() {
        assert_eq!(list_continuation("これは普通の文章です。"), None);
    }

    #[test]
    fn 見出しの行はnone() {
        // 見出しの改行はリスト継続とは無関係。地の文と同じくインデント維持だけでよい。
        assert_eq!(list_continuation("# 見出し"), None);
    }

    #[test]
    fn 空行はnone() {
        assert_eq!(list_continuation(""), None);
    }

    #[test]
    fn 空白だけの行はnone() {
        assert_eq!(list_continuation("   "), None);
    }

    #[test]
    fn 水平線をリストと誤判定しない() {
        // "---" は先頭 2 文字が "--" でハイフン+スペースの箇条書きマーカーと
        // 一致しないため、自然に弾かれる。
        assert_eq!(list_continuation("---"), None);
        assert_eq!(list_continuation("***"), None);
        assert_eq!(list_continuation("___"), None);
    }

    #[test]
    fn 行の途中で改行しても項目全体で継続を判定する() {
        // on_newline は選択開始位置の行全体をこの関数に渡す。カーソルが項目の
        // 途中にあっても、行にマーカー以外の中身が残っていれば継続と判定してよい
        // (カーソルより後ろの文字は改行後の新しい行にそのまま移る)。
        assert_eq!(
            list_continuation("- 前半と後半"),
            Some(ListContinuation::Continue("- ".to_string()))
        );
    }

    #[test]
    fn 全角文字を含む項目が継続する() {
        assert_eq!(
            list_continuation("- こんにちは世界"),
            Some(ListContinuation::Continue("- ".to_string()))
        );
    }

    #[test]
    fn 全角文字を含むチェックリストが継続する() {
        assert_eq!(
            list_continuation("- [x] 買い物リストを確認する"),
            Some(ListContinuation::Continue("- [ ] ".to_string()))
        );
    }

    #[test]
    fn スペースを省いた引用も継続する() {
        // ">" の直後にスペースが無くても引用として扱う (CommonMark の遅延継続と同じ)。
        assert_eq!(
            list_continuation(">引用文"),
            Some(ListContinuation::Continue("> ".to_string()))
        );
    }

    #[test]
    fn 区切り文字だけの数字付きリストはnone() {
        // "1)" のように区切り文字の後にスペースが無ければ番号付きリストとは
        // 認識しない。書きかけの数字と区別が付かないため。
        assert_eq!(list_continuation("1)text"), None);
    }
}
