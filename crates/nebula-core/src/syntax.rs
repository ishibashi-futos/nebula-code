//! Tree-sitter によるインクリメンタル構文解析とハイライトスパン生成。
//!
//! 解析対象のテキストは `Rope` のまま渡す。`to_string()` で平坦化すると
//! 1 打鍵ごとにファイル全体の複製が発生し、大きなファイルで目に見えて遅くなる。

use crate::buffer::EditRecord;
use crate::language::CompiledLanguage;
use nebula_protocol::{HighlightSpan, TokenKind};
use ropey::Rope;
use std::ops::Range;
use std::sync::Arc;
use tree_sitter::{
    InputEdit, Node, Parser, Point, Query, QueryCursor, StreamingIterator, TextProvider, Tree,
};

/// `Rope` の一部をチャンク列として tree-sitter に渡すためのアダプタ。
struct ChunksBytes<'a>(ropey::iter::Chunks<'a>);

impl<'a> Iterator for ChunksBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(str::as_bytes)
    }
}

struct RopeProvider<'a>(&'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeProvider<'a> {
    type I = ChunksBytes<'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let range = node.byte_range();
        let end = range.end.min(self.0.len_bytes());
        let start = range.start.min(end);
        ChunksBytes(self.0.byte_slice(start..end).chunks())
    }
}

/// 1 バッファぶんの構文解析状態。
pub struct SyntaxTree {
    language: Arc<CompiledLanguage>,
    parser: Parser,
    tree: Option<Tree>,
}

impl SyntaxTree {
    pub fn new(language: Arc<CompiledLanguage>) -> Result<Self, tree_sitter::LanguageError> {
        let mut parser = Parser::new();
        parser.set_language(&language.grammar)?;
        Ok(Self {
            language,
            parser,
            tree: None,
        })
    }

    pub fn language(&self) -> &Arc<CompiledLanguage> {
        &self.language
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// 編集を木に通知する。`reparse` の前に、適用したのと同じ順序で呼ぶ。
    pub fn apply_edit(&mut self, record: &EditRecord) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        tree.edit(&InputEdit {
            start_byte: record.start_byte,
            old_end_byte: record.old_end_byte,
            new_end_byte: record.new_end_byte,
            start_position: Point::new(record.start_point.row, record.start_point.column),
            old_end_position: Point::new(record.old_end_point.row, record.old_end_point.column),
            new_end_position: Point::new(record.new_end_point.row, record.new_end_point.column),
        });
    }

    /// 解析し直す。直前の木があればインクリメンタル解析になる。
    pub fn reparse(&mut self, rope: &Rope) {
        let new_tree = self.parser.parse_with_options(
            &mut |byte_offset: usize, _: Point| -> &[u8] {
                if byte_offset >= rope.len_bytes() {
                    return &[];
                }
                // チャンク境界をまたがずに、要求位置から chunk の末尾までを返す。
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte_offset);
                &chunk.as_bytes()[byte_offset - chunk_start..]
            },
            self.tree.as_ref(),
            None,
        );
        // `parse_with_options` が `None` を返すのは言語未設定か中断時のみ。
        // 中断していない以上ここでは起きないが、起きたときは古い木を保持し続ける。
        if let Some(tree) = new_tree {
            self.tree = Some(tree);
        }
    }

    /// 指定した行範囲のハイライトスパンを、**文字オフセット**で返す。
    ///
    /// 返るスパンは重なりがなく、開始位置の昇順に並ぶ。描画側はそのまま
    /// 順に色を塗るだけでよい。
    pub fn highlights(&self, rope: &Rope, rows: Range<u32>) -> Vec<HighlightSpan> {
        let Some(tree) = self.tree.as_ref() else {
            return Vec::new();
        };
        // `rows` は終端排他。`rows.end` 行目そのものは含めない。
        let line_count = rope.len_lines();
        let start_row = (rows.start as usize).min(line_count.saturating_sub(1));
        let start_char = rope.line_to_char(start_row);
        let end_char = if (rows.end as usize) >= line_count {
            rope.len_chars()
        } else {
            rope.line_to_char(rows.end as usize)
        };
        if start_char >= end_char {
            return Vec::new();
        }

        let captures = self.collect_captures(
            tree,
            rope,
            rope.char_to_byte(start_char)..rope.char_to_byte(end_char),
        );
        paint_spans(captures, start_char, end_char)
    }

    fn collect_captures(
        &self,
        tree: &Tree,
        rope: &Rope,
        byte_range: Range<usize>,
    ) -> Vec<RawCapture> {
        let query: &Query = &self.language.highlights;
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(byte_range);
        let mut captures = cursor.captures(query, tree.root_node(), RopeProvider(rope));

        let mut collected = Vec::new();
        while let Some((query_match, capture_index)) = captures.next() {
            let capture = query_match.captures[*capture_index];
            let Some(Some(token)) = self
                .language
                .capture_tokens
                .get(capture.index as usize)
                .copied()
            else {
                continue;
            };
            let node_range = capture.node.byte_range();
            collected.push(RawCapture {
                start: rope.byte_to_char(node_range.start.min(rope.len_bytes())),
                end: rope.byte_to_char(node_range.end.min(rope.len_bytes())),
                pattern_index: query_match.pattern_index,
                token,
            });
        }
        collected
    }
}

#[derive(Debug, Clone, Copy)]
struct RawCapture {
    start: usize,
    end: usize,
    pattern_index: usize,
    token: TokenKind,
}

/// 重なり合うキャプチャを、重なりのないスパン列に解決する。
///
/// tree-sitter の慣例に合わせて (1) 内側 (短い) のキャプチャが外側に勝ち、
/// (2) 同じ範囲ならクエリ内で先に書かれたパターンが勝つ。
/// 塗り絵方式で解決するので、優先度の低いものから順に上書きしていく。
fn paint_spans(mut captures: Vec<RawCapture>, start_char: usize, end_char: usize) -> Vec<HighlightSpan> {
    if captures.is_empty() {
        return Vec::new();
    }
    captures.sort_by(|a, b| {
        let a_len = a.end.saturating_sub(a.start);
        let b_len = b.end.saturating_sub(b.start);
        // 長いものが先 (=先に塗られて後から上書きされる)。
        b_len
            .cmp(&a_len)
            // 同じ長さならパターン索引が大きいものを先に塗り、小さいものを後に塗る。
            .then(b.pattern_index.cmp(&a.pattern_index))
    });

    let width = end_char - start_char;
    let mut paint: Vec<Option<TokenKind>> = vec![None; width];
    for capture in captures {
        let from = capture.start.max(start_char).saturating_sub(start_char);
        let to = capture.end.min(end_char).saturating_sub(start_char);
        for slot in &mut paint[from..to.max(from)] {
            *slot = Some(capture.token);
        }
    }

    // 連続する同一トークンを 1 スパンに畳む。
    let mut spans: Vec<HighlightSpan> = Vec::new();
    let mut run_start = 0usize;
    let mut run_token = paint.first().copied().flatten();
    for i in 1..=width {
        let token = if i < width { paint[i] } else { None };
        if token != run_token || i == width {
            if let Some(kind) = run_token {
                spans.push(HighlightSpan {
                    start: start_char + run_start,
                    end: start_char + i,
                    token: kind,
                });
            }
            run_start = i;
            run_token = token;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextBuffer;
    use crate::language::{LanguageRegistry, language_by_id};
    use crate::selection::Selection;
    use nebula_protocol::{Edit, TextRange};

    fn tree_for(language: &str, text: &str) -> (SyntaxTree, Rope) {
        let registry = LanguageRegistry::new();
        let compiled = registry.get(language_by_id(language).unwrap()).unwrap();
        let mut syntax = SyntaxTree::new(compiled).unwrap();
        let rope = Rope::from_str(text);
        syntax.reparse(&rope);
        (syntax, rope)
    }

    fn token_at(spans: &[HighlightSpan], offset: usize) -> Option<TokenKind> {
        spans
            .iter()
            .find(|s| s.start <= offset && offset < s.end)
            .map(|s| s.token)
    }

    #[test]
    fn rust_のキーワードと文字列が色分けされる() {
        let src = "fn main() {\n    let s = \"hi\";\n}\n";
        let (syntax, rope) = tree_for("rust", src);
        let spans = syntax.highlights(&rope, 0..3);
        assert_eq!(token_at(&spans, 0), Some(TokenKind::Keyword), "fn");
        let string_offset = src.find("\"hi\"").unwrap();
        assert_eq!(
            token_at(&spans, string_offset),
            Some(TokenKind::String),
            "文字列リテラル"
        );
    }

    #[test]
    fn スパンは重ならず昇順に並ぶ() {
        let src = "fn f<T: Copy>(x: T) -> T { x }\n";
        let (syntax, rope) = tree_for("rust", src);
        let spans = syntax.highlights(&rope, 0..2);
        assert!(!spans.is_empty());
        for pair in spans.windows(2) {
            assert!(
                pair[0].end <= pair[1].start,
                "重なりがある: {:?} と {:?}",
                pair[0],
                pair[1]
            );
        }
        for span in &spans {
            assert!(span.start < span.end, "空スパンが混じっている");
        }
    }

    #[test]
    fn 要求した行範囲の外にスパンが出ない() {
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (syntax, rope) = tree_for("rust", src);
        let spans = syntax.highlights(&rope, 1..2);
        let line1_start = rope.line_to_char(1);
        let line2_start = rope.line_to_char(2);
        for span in &spans {
            assert!(
                span.start >= line1_start && span.end <= line2_start,
                "範囲外のスパン: {span:?}"
            );
        }
        assert!(!spans.is_empty(), "2 行目に何も色がつかないのはおかしい");
    }

    #[test]
    fn 編集後にインクリメンタル解析で追従する() {
        let registry = LanguageRegistry::new();
        let compiled = registry.get(language_by_id("rust").unwrap()).unwrap();
        let mut syntax = SyntaxTree::new(compiled).unwrap();
        let mut buffer = TextBuffer::new("fn main() {}\n");
        syntax.reparse(buffer.rope());

        let sels = vec![Selection::caret(0)];
        let records = buffer
            .edit(
                &[Edit::insert(11, "\n    let x = 1;\n")],
                &sels,
                &sels,
            )
            .unwrap();
        for record in &records {
            syntax.apply_edit(record);
        }
        syntax.reparse(buffer.rope());

        let text = buffer.text();
        let let_offset = text.find("let").unwrap();
        let spans = syntax.highlights(buffer.rope(), 0..5);
        assert_eq!(token_at(&spans, let_offset), Some(TokenKind::Keyword));
        assert!(
            !syntax.tree().unwrap().root_node().has_error(),
            "編集後の木に構文エラーが残っている"
        );
    }

    #[test]
    fn マルチバイト文字を含む行でも位置がずれない() {
        let src = "// 日本語のコメント\nfn main() {}\n";
        let (syntax, rope) = tree_for("rust", src);
        let spans = syntax.highlights(&rope, 0..2);
        assert_eq!(token_at(&spans, 0), Some(TokenKind::Comment));
        let fn_offset = src.chars().take_while(|c| *c != '\n').count() + 1;
        assert_eq!(
            token_at(&spans, fn_offset),
            Some(TokenKind::Keyword),
            "2 行目の fn が文字オフセットで正しく指せている"
        );
    }

    #[test]
    fn 空バッファでも落ちない() {
        let (syntax, rope) = tree_for("rust", "");
        assert!(syntax.highlights(&rope, 0..1).is_empty());
    }

    #[test]
    fn typescript_も解析できる() {
        let src = "const x: number = 1;\n";
        let (syntax, rope) = tree_for("typescript", src);
        let spans = syntax.highlights(&rope, 0..1);
        assert!(!spans.is_empty());
    }

    #[test]
    fn 削除編集にも追従する() {
        let registry = LanguageRegistry::new();
        let compiled = registry.get(language_by_id("rust").unwrap()).unwrap();
        let mut syntax = SyntaxTree::new(compiled).unwrap();
        let mut buffer = TextBuffer::new("fn a() {}\nfn b() {}\n");
        syntax.reparse(buffer.rope());

        let sels = vec![Selection::caret(0)];
        let records = buffer
            .edit(&[Edit::delete(TextRange::new(0, 10))], &sels, &sels)
            .unwrap();
        for record in &records {
            syntax.apply_edit(record);
        }
        syntax.reparse(buffer.rope());
        assert_eq!(buffer.text(), "fn b() {}\n");
        assert!(!syntax.tree().unwrap().root_node().has_error());
    }
}
