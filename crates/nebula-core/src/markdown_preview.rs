//! Markdown プレビュー用の構造化。
//!
//! Markdown のソース文字列を、プレビューに描くべき要素の並び ([`PreviewBlock`]) へ
//! 変換する。GPUI にも `TextBuffer`/`Rope` にも触れない純粋関数として作ってあるので、
//! GUI を起動しなくても変換の正しさをテストできる。
//!
//! `PreviewBlock`/`ListMarker` そのものは IPC を跨ぐため `nebula-protocol` 側に
//! 置かれている ([`HighlightSpan`](nebula_protocol::HighlightSpan) と同じ理由)。
//! この関数を呼ぶのはバックエンド (`crates/nebula-backend/src/buffers.rs`) だけで、
//! 結果は `Response::MarkdownPreview` として GUI へ渡る。描画そのもの (div の組み立て)
//! は GUI 側 (`crates/nebula/src/views/markdown_preview.rs`) の仕事で、ここでは扱わない。
//!
//! パーサは自前で書かず、`language.rs` の markdown 言語定義が使っているのと同じ
//! `tree-sitter-md` のブロック文法 (`tree_sitter_md::LANGUAGE`) をそのまま使う。
//! 行頭の記号を正規表現的に読むような手書きパーサは、CommonMark のブロック規則
//! (継続行・ネスト・遅延継続など) を再実装することになり保守が大変なので避けている。
//!
//! ## 対応範囲 (最小構成)
//!
//! 見出し (ATX `#`・Setext `===`/`---`)・箇条書き (ネスト含む)・チェックリスト・
//! 順序付きリスト・フェンス付きコードブロック・引用・段落・水平線だけを扱う。
//! 以下は非対応で、該当ノードは黙って読み飛ばす (プレビューにそのブロックが
//! 出てこないだけで、他の部分の変換は壊れない):
//!
//! - インライン装飾 (`**太字**`・`*斜体*`・`` `コード` ``・リンクなど)。
//!   `tree_sitter_md::INLINE_LANGUAGE` を使えば対応できるが、最小構成の対象外とした。
//!   段落・見出し・リスト項目のテキストは、Markdown 記法込みの生の文字列をそのまま返す。
//! - 4 字下げによる (フェンスなしの) コードブロック。今日ではフェンス付きが主流。
//! - リスト項目内の複数段落・コードブロック・引用 (「ゆるいリスト」)。
//!   各項目の最初の段落だけを項目のテキストとして使う。
//! - 引用の中に見出し・リスト・コードブロックが入れ子になっているケース。
//!   引用の中の段落は [`PreviewBlock::Quote`] になるが、それ以外のブロック種別は
//!   引用の外にあるのと同じように (深さ情報を持たずに) 変換される。
//! - テーブル・HTML ブロック・リンク参照定義・YAML フロントマター。

use nebula_protocol::{ListMarker, PreviewBlock};
use tree_sitter::{Node, Parser};

/// Markdown のソース文字列を、プレビューに描くべき要素の並びへ変換する。
///
/// 空文字列・空白だけの入力は空の `Vec` を返す (プレビュー側は「内容がありません」
/// のような案内を出せばよい)。構文エラーがあっても tree-sitter は壊れた部分を
/// 読み飛ばして木を返すので、ここでは panic しない。
pub fn parse_preview(source: &str) -> Vec<PreviewBlock> {
    let mut parser = Parser::new();
    // `tree_sitter_md::LANGUAGE` は静的に定義された文法なので、設定が失敗するのは
    // ビルド構成が壊れているとき (文法バージョンの不一致など) だけ。実行時の入力には
    // 依存しないため、ここでの失敗は呼び出し元でリカバリさせず早期に気付けるようにする。
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .expect("tree-sitter-md の文法設定に失敗した (ビルド構成を確認すること)");
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    walk_container(tree.root_node(), source, 0, &mut blocks);
    blocks
}

/// `document` / `section` / `block_quote` に共通する「子ノードを順に見て種類ごとに
/// 振り分ける」処理。tree-sitter-md は見出し配下の内容を入れ子の `section` として
/// 表現する (`# a` の直後の `section` に `## b` 以降がまとめて入る) が、その `section`
/// も同じ関数で再帰的に辿るので、結果は読み順のまま平坦な列になる。
fn walk_container(node: Node, source: &str, quote_depth: u8, out: &mut Vec<PreviewBlock>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_block(child, source, quote_depth, out);
    }
}

fn walk_block(node: Node, source: &str, quote_depth: u8, out: &mut Vec<PreviewBlock>) {
    match node.kind() {
        "document" | "section" => walk_container(node, source, quote_depth, out),
        "atx_heading" => {
            if let Some(level) = atx_heading_level(node) {
                let text = node
                    .child_by_field_name("heading_content")
                    .map(|inline| flatten_inline(inline, source))
                    .unwrap_or_default();
                out.push(PreviewBlock::Heading { level, text });
            }
        }
        "setext_heading" => {
            let level = setext_heading_level(node);
            let text = node
                .child_by_field_name("heading_content")
                .map(|paragraph| paragraph_text(paragraph, source))
                .unwrap_or_default();
            out.push(PreviewBlock::Heading { level, text });
        }
        "paragraph" => {
            let text = paragraph_text(node, source);
            if quote_depth > 0 {
                out.push(PreviewBlock::Quote {
                    depth: quote_depth,
                    text,
                });
            } else {
                out.push(PreviewBlock::Paragraph { text });
            }
        }
        "list" => walk_list(node, source, 0, out),
        "block_quote" => walk_container(node, source, quote_depth + 1, out),
        "fenced_code_block" => out.push(fenced_code_block(node, source)),
        "thematic_break" => out.push(PreviewBlock::ThematicBreak),
        // インデント式コードブロック・HTML ブロック・テーブル・リンク参照定義・
        // フロントマターは最小構成の対象外。黙って読み飛ばす。
        _ => {}
    }
}

/// `list` ノードの直下の `list_item` を順に処理する。
fn walk_list(node: Node, source: &str, depth: u8, out: &mut Vec<PreviewBlock>) {
    let mut cursor = node.walk();
    for item in node.children(&mut cursor) {
        if item.kind() == "list_item" {
            walk_list_item(item, source, depth, out);
        }
    }
}

/// 1 つのリスト項目。項目自身のテキストは直下の最初の `paragraph` から取り、
/// ネストしたリストがあれば深さを 1 つ増やして続けて処理する。
fn walk_list_item(node: Node, source: &str, depth: u8, out: &mut Vec<PreviewBlock>) {
    let Some(marker) = list_item_marker(node, source) else {
        return;
    };
    let mut cursor = node.walk();
    let text = node
        .children(&mut cursor)
        .find(|c| c.kind() == "paragraph")
        .map(|p| paragraph_text(p, source))
        .unwrap_or_default();
    out.push(PreviewBlock::ListItem { depth, marker, text });

    let mut cursor = node.walk();
    if let Some(nested) = node.children(&mut cursor).find(|c| c.kind() == "list") {
        walk_list(nested, source, depth + 1, out);
    }
}

/// リスト項目の行頭記号を判定する。チェックリストの `[ ]`/`[x]` は箇条書きの
/// マーカー (`list_marker_minus` 等) の後ろに別ノードとして現れるので、
/// 先に箇条書き/順序付きを仮に決めておき、チェックマーカーが見つかれば上書きする。
fn list_item_marker(node: Node, source: &str) -> Option<ListMarker> {
    let mut marker: Option<ListMarker> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "list_marker_minus" | "list_marker_star" | "list_marker_plus" => {
                marker = Some(ListMarker::Bullet);
            }
            "list_marker_dot" | "list_marker_parenthesis" => {
                let text = child.utf8_text(source.as_bytes()).unwrap_or_default();
                marker = Some(ListMarker::Ordered(leading_number(text)));
            }
            "task_list_marker_unchecked" => marker = Some(ListMarker::TaskUnchecked),
            "task_list_marker_checked" => marker = Some(ListMarker::TaskChecked),
            _ => {}
        }
    }
    marker
}

/// マーカー文字列 (`"5. "` など) の先頭の数字部分を読む。
/// 文法上、数字が 1 桁も無いマーカーは `list_marker_dot`/`list_marker_parenthesis`
/// として認識されないので、パース失敗時の `1` はまず通らない安全策。
fn leading_number(marker_text: &str) -> u64 {
    let digits: String = marker_text
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(1)
}

fn atx_heading_level(node: Node) -> Option<u8> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find_map(|c| match c.kind() {
        "atx_h1_marker" => Some(1),
        "atx_h2_marker" => Some(2),
        "atx_h3_marker" => Some(3),
        "atx_h4_marker" => Some(4),
        "atx_h5_marker" => Some(5),
        "atx_h6_marker" => Some(6),
        _ => None,
    })
}

/// Setext 見出しの下線 (`===` なら 1、`---` なら 2) からレベルを決める。
/// 下線ノードが (壊れた入力などで) 見当たらない場合は 1 に倒す。
fn setext_heading_level(node: Node) -> u8 {
    let mut cursor = node.walk();
    if node
        .children(&mut cursor)
        .any(|c| c.kind() == "setext_h2_underline")
    {
        2
    } else {
        1
    }
}

fn fenced_code_block(node: Node, source: &str) -> PreviewBlock {
    let mut language = None;
    let mut code = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "info_string" => {
                let text = node_text_without_continuation(child, source);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    language = Some(trimmed.to_string());
                }
            }
            "code_fence_content" => {
                // コードは体裁を保つ必要があるので、段落と違って行の trim も
                // 空白の折り畳みもしない (block_continuation の除去だけ行う)。
                let text = node_text_without_continuation(child, source);
                code = text.trim_end_matches('\n').to_string();
            }
            _ => {}
        }
    }
    PreviewBlock::CodeBlock { language, code }
}

/// `paragraph` ノードから、直下の `inline` 子のテキストを取り出す。
/// `paragraph` は `inline` 以外に (行継続を表す) `block_continuation` を
/// 直接の子として持つことがあるが、`inline` の範囲だけを見るので影響しない。
fn paragraph_text(node: Node, source: &str) -> String {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|c| c.kind() == "inline")
        .map(|inline| flatten_inline(inline, source))
        .unwrap_or_default()
}

/// `inline` ノードのテキストを、行継続の字下げを取り除いたうえで 1 行に平坦化する。
///
/// Markdown のソフト改行 (段落やリスト項目の中の単独の改行) は CommonMark の
/// 仕様上そのまま空白 1 個として描画するのが正しい (見た目上の折り返しに過ぎず、
/// 強制改行ではない) ので、ここで潰しておく。
///
/// `str::split_whitespace` を使わないのは、それだと全角スペース (U+3000) も
/// 空白として split されてしまい、全角文字を含む行の中身までおかしくなるため。
/// 行の**先頭と末尾**だけを trim し、行同士を半角スペース 1 個でつなぐことで、
/// 行の途中にある空白 (全角・半角問わず) はそのまま残す。
fn flatten_inline(node: Node, source: &str) -> String {
    let raw = node_text_without_continuation(node, source);
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// ノードのテキストから `block_continuation` (引用の `"> "` や、複数行にまたがる
/// リスト項目の字下げなど、行継続のために挿入される部分) を取り除いて返す。
///
/// ソースをそのまま切り出すと、複数行にまたがる引用・リスト項目の 2 行目以降に
/// `"> "` や字下げがそのまま紛れ込んでしまう (`block_continuation` は対象ノードの
/// 直接の子として、あるいは子の `inline` のさらに子として現れる。ここでは
/// 直接の子だけを見れば十分で、`inline` 自身に対して呼べば `inline` の中の
/// `block_continuation` を取り除ける)。
fn node_text_without_continuation(node: Node, source: &str) -> String {
    let mut text = String::new();
    let mut pos = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block_continuation" {
            text.push_str(&source[pos..child.start_byte()]);
            pos = child.end_byte();
        }
    }
    text.push_str(&source[pos..node.end_byte()]);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 空入力は空になる() {
        assert_eq!(parse_preview(""), Vec::new());
    }

    #[test]
    fn 空白だけの入力は空になる() {
        assert_eq!(parse_preview("   \n\n\t\n"), Vec::new());
    }

    #[test]
    fn atx見出しレベル1から6までを認識する() {
        let src = "# h1\n## h2\n### h3\n#### h4\n##### h5\n###### h6\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::Heading { level: 1, text: "h1".into() },
                PreviewBlock::Heading { level: 2, text: "h2".into() },
                PreviewBlock::Heading { level: 3, text: "h3".into() },
                PreviewBlock::Heading { level: 4, text: "h4".into() },
                PreviewBlock::Heading { level: 5, text: "h5".into() },
                PreviewBlock::Heading { level: 6, text: "h6".into() },
            ]
        );
    }

    #[test]
    fn setext見出しはイコールがレベル1でハイフンがレベル2() {
        let src = "見出し1\n====\n\n見出し2\n----\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::Heading { level: 1, text: "見出し1".into() },
                PreviewBlock::Heading { level: 2, text: "見出し2".into() },
            ]
        );
    }

    #[test]
    fn 中身の無い見出しはテキストが空文字になる() {
        assert_eq!(
            parse_preview("###\n"),
            vec![PreviewBlock::Heading { level: 3, text: String::new() }]
        );
    }

    #[test]
    fn 段落を認識する() {
        assert_eq!(
            parse_preview("ただの段落です。\n"),
            vec![PreviewBlock::Paragraph { text: "ただの段落です。".into() }]
        );
    }

    #[test]
    fn 段落中のソフト改行は空白1個に潰れる() {
        // CommonMark 上、段落内の単独の改行はソフト改行 (見た目の折り返し) であって
        // 強制改行ではないので、空白として描画するのが正しい。
        assert_eq!(
            parse_preview("1行目\n2行目です。\n"),
            vec![PreviewBlock::Paragraph { text: "1行目 2行目です。".into() }]
        );
    }

    #[test]
    fn 段落中の全角スペースは潰れずに残る() {
        // split_whitespace で行継続後の文字列を再分割すると、全角スペース (U+3000)
        // も区切りとして扱われてしまい半角スペースに化ける。行の trim + 半角
        // スペースでの再連結なら、行の**途中**にある全角スペースはそのまま残る。
        assert_eq!(
            parse_preview("全角　スペースを含む段落\n"),
            vec![PreviewBlock::Paragraph { text: "全角　スペースを含む段落".into() }]
        );
    }

    #[test]
    fn 全角文字を含む文書を扱える() {
        let src = "# 日本語の見出し\n\n段落のテキストです。\n\n- 箇条書きの項目\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::Heading { level: 1, text: "日本語の見出し".into() },
                PreviewBlock::Paragraph { text: "段落のテキストです。".into() },
                PreviewBlock::ListItem {
                    depth: 0,
                    marker: ListMarker::Bullet,
                    text: "箇条書きの項目".into(),
                },
            ]
        );
    }

    #[test]
    fn 箇条書きの記号はハイフンアスタリスクプラスのどれでも認識する() {
        for marker in ["-", "*", "+"] {
            let src = format!("{marker} 項目\n");
            assert_eq!(
                parse_preview(&src),
                vec![PreviewBlock::ListItem {
                    depth: 0,
                    marker: ListMarker::Bullet,
                    text: "項目".into(),
                }],
                "マーカー {marker} で失敗"
            );
        }
    }

    #[test]
    fn ネストした箇条書きは深さが1つ増える() {
        let src = "- 親項目\n  - 子項目\n    - 孫項目\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::ListItem {
                    depth: 0,
                    marker: ListMarker::Bullet,
                    text: "親項目".into(),
                },
                PreviewBlock::ListItem {
                    depth: 1,
                    marker: ListMarker::Bullet,
                    text: "子項目".into(),
                },
                PreviewBlock::ListItem {
                    depth: 2,
                    marker: ListMarker::Bullet,
                    text: "孫項目".into(),
                },
            ]
        );
    }

    #[test]
    fn ネストの後兄弟項目に戻ると深さも戻る() {
        let src = "- 親1\n  - 子\n- 親2\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Bullet, text: "親1".into() },
                PreviewBlock::ListItem { depth: 1, marker: ListMarker::Bullet, text: "子".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Bullet, text: "親2".into() },
            ]
        );
    }

    #[test]
    fn 順序付きリストは番号をそのまま保持する() {
        assert_eq!(
            parse_preview("1. 一つ目\n2. 二つ目\n"),
            vec![
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Ordered(1), text: "一つ目".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Ordered(2), text: "二つ目".into() },
            ]
        );
    }

    #[test]
    fn 順序付きリストは5から始まっても番号をそのまま保持する() {
        assert_eq!(
            parse_preview("5. five\n6. six\n"),
            vec![
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Ordered(5), text: "five".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Ordered(6), text: "six".into() },
            ]
        );
    }

    #[test]
    fn 括弧区切りの順序付きリストも認識する() {
        assert_eq!(
            parse_preview("1) one\n"),
            vec![PreviewBlock::ListItem { depth: 0, marker: ListMarker::Ordered(1), text: "one".into() }]
        );
    }

    #[test]
    fn チェックリストは未チェックとチェック済みを区別する() {
        assert_eq!(
            parse_preview("- [ ] 未完了\n- [x] 完了\n"),
            vec![
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::TaskUnchecked, text: "未完了".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::TaskChecked, text: "完了".into() },
            ]
        );
    }

    #[test]
    fn コードブロックは言語指定を保持する() {
        assert_eq!(
            parse_preview("```rust\nfn main() {}\n```\n"),
            vec![PreviewBlock::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn コードブロックは言語指定が無くても中身を保持する() {
        assert_eq!(
            parse_preview("```\nplain text\n```\n"),
            vec![PreviewBlock::CodeBlock {
                language: None,
                code: "plain text".into(),
            }]
        );
    }

    #[test]
    fn コードブロックは複数行と空行をそのまま保持する() {
        let src = "```rust\nfn main() {\n    let x = 1;\n\n    println!(\"{x}\");\n}\n```\n";
        assert_eq!(
            parse_preview(src),
            vec![PreviewBlock::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {\n    let x = 1;\n\n    println!(\"{x}\");\n}".into(),
            }]
        );
    }

    #[test]
    fn 空のコードブロックはコードが空文字になる() {
        assert_eq!(
            parse_preview("```\n```\n"),
            vec![PreviewBlock::CodeBlock { language: None, code: String::new() }]
        );
    }

    #[test]
    fn 引用を認識する() {
        assert_eq!(
            parse_preview("> 引用文\n"),
            vec![PreviewBlock::Quote { depth: 1, text: "引用文".into() }]
        );
    }

    #[test]
    fn 複数行の引用はソフト改行を空白に潰して1つにまとまる() {
        assert_eq!(
            parse_preview("> 1行目\n> 2行目\n"),
            vec![PreviewBlock::Quote { depth: 1, text: "1行目 2行目".into() }]
        );
    }

    #[test]
    fn 入れ子の引用は深さが2になる() {
        assert_eq!(
            parse_preview("> > ネスト引用\n"),
            vec![PreviewBlock::Quote { depth: 2, text: "ネスト引用".into() }]
        );
    }

    #[test]
    fn 水平線を認識する() {
        for src in ["---\n", "***\n", "___\n"] {
            assert_eq!(parse_preview(src), vec![PreviewBlock::ThematicBreak], "{src} で失敗");
        }
    }

    #[test]
    fn 見出しと段落とリストが混在する文書を読み順のまま変換する() {
        let src = "# タイトル\n\n段落です。\n\n- 項目1\n- 項目2\n\n## 次の見出し\n\n> 引用\n";
        assert_eq!(
            parse_preview(src),
            vec![
                PreviewBlock::Heading { level: 1, text: "タイトル".into() },
                PreviewBlock::Paragraph { text: "段落です。".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Bullet, text: "項目1".into() },
                PreviewBlock::ListItem { depth: 0, marker: ListMarker::Bullet, text: "項目2".into() },
                PreviewBlock::Heading { level: 2, text: "次の見出し".into() },
                PreviewBlock::Quote { depth: 1, text: "引用".into() },
            ]
        );
    }
}
