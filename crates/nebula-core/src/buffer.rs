//! Rope を土台にしたテキストバッファ。編集・Undo/Redo・版数管理を担う。
//!
//! GUI プロセスとバックエンドプロセスの双方がこの型を使う。GUI 側は即時描画のための
//! 複製として、バックエンド側は保存・LSP 同期・構文解析の基準となる正本として保持する。

use crate::error::{CoreError, Result};
use crate::rope_ext::RopeExt;
use crate::selection::Selection;
use nebula_protocol::{Edit, LineEnding, TextRange};
use ropey::Rope;

/// 構文解析器へ渡すための、1 回の編集の位置情報。
///
/// tree-sitter の `InputEdit` はバイト単位かつ「行, 行内バイト」の点で表すため、
/// 文字単位で扱うバッファ側とは座標系が違う。変換をここで済ませておく。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRecord {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: BytePoint,
    pub old_end_point: BytePoint,
    pub new_end_point: BytePoint,
}

/// 行番号と行内バイトオフセットによる位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytePoint {
    pub row: usize,
    pub column: usize,
}

/// Undo/Redo の 1 単位。
///
/// 1 トランザクションは複数の `edit()` 呼び出し (ステップ) を含みうる。
/// 例えば「選択範囲を削除してから文字を挿入する」入力は 2 ステップだが、
/// ユーザーから見れば 1 回の Undo で戻るべき 1 操作。
#[derive(Debug, Clone)]
struct Transaction {
    steps: Vec<EditStep>,
    selections_before: Vec<Selection>,
    selections_after: Vec<Selection>,
}

#[derive(Debug, Clone)]
struct EditStep {
    /// 適用順 (文書内で後ろの編集が先) に並んだ順方向の編集。
    forward: Vec<Edit>,
    /// `forward` と同じ索引で対応する逆編集。同じ順序で適用すれば元に戻る。
    inverse: Vec<Edit>,
}

#[derive(Debug, Default)]
struct History {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    open: Option<Transaction>,
    /// 最後に保存した時点の `undo` の深さ。Undo で保存時点まで戻れば未変更に戻す。
    saved_depth: Option<usize>,
}

#[derive(Debug)]
pub struct TextBuffer {
    rope: Rope,
    version: u64,
    line_ending: LineEnding,
    history: History,
}

impl TextBuffer {
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let line_ending = rope.detect_line_ending();
        Self {
            rope,
            version: 0,
            line_ending,
            history: History {
                saved_depth: Some(0),
                ..History::default()
            },
        }
    }

    pub fn empty() -> Self {
        Self::new("")
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// 保存後の内容から変更されているか。
    pub fn is_dirty(&self) -> bool {
        self.history.open.is_some() || self.history.saved_depth != Some(self.history.undo.len())
    }

    /// 現在の内容を保存済みとして記録する。
    pub fn mark_saved(&mut self) {
        self.commit();
        self.history.saved_depth = Some(self.history.undo.len());
    }

    pub fn can_undo(&self) -> bool {
        self.history.open.is_some() || !self.history.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.history.redo.is_empty()
    }

    /// 外部からの再読込。履歴は破棄され、保存済み状態になる。
    pub fn reset(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.line_ending = self.rope.detect_line_ending();
        self.version += 1;
        self.history = History {
            saved_depth: Some(0),
            ..History::default()
        };
    }

    /// 版数を検査したうえで編集を適用する。
    pub fn apply_versioned(
        &mut self,
        base_version: u64,
        edits: &[Edit],
        selections_before: &[Selection],
        selections_after: &[Selection],
    ) -> Result<Vec<EditRecord>> {
        if base_version != self.version {
            return Err(CoreError::VersionMismatch {
                expected: self.version,
                actual: base_version,
            });
        }
        self.edit(edits, selections_before, selections_after)
    }

    /// 編集を適用し、構文解析器へ渡すための位置情報を返す。
    ///
    /// 渡された編集の範囲はすべて **適用前** の座標系で解釈される。重なる編集は拒否する。
    pub fn edit(
        &mut self,
        edits: &[Edit],
        selections_before: &[Selection],
        selections_after: &[Selection],
    ) -> Result<Vec<EditRecord>> {
        if edits.is_empty() {
            return Ok(Vec::new());
        }
        let sorted = self.validate_and_sort(edits)?;

        let mut inverse = Vec::with_capacity(sorted.len());
        let mut records = Vec::with_capacity(sorted.len());
        for edit in &sorted {
            let (inv, record) = self.apply_one(edit);
            inverse.push(inv);
            records.push(record);
        }

        let step = EditStep {
            forward: sorted,
            inverse,
        };
        match &mut self.history.open {
            Some(open) => {
                open.steps.push(step);
                open.selections_after = selections_after.to_vec();
            }
            None => {
                self.history.open = Some(Transaction {
                    steps: vec![step],
                    selections_before: selections_before.to_vec(),
                    selections_after: selections_after.to_vec(),
                });
            }
        }
        // 新しい編集が入った時点で Redo 履歴は無効になる。
        self.history.redo.clear();
        self.version += 1;
        Ok(records)
    }

    /// 開いているトランザクションを確定する。以降の編集は別の Undo 単位になる。
    pub fn commit(&mut self) {
        if let Some(open) = self.history.open.take() {
            self.history.undo.push(open);
        }
    }

    /// 直前のトランザクションを取り消す。復元すべき選択範囲を返す。
    pub fn undo(&mut self) -> Option<(Vec<Selection>, Vec<EditRecord>)> {
        self.commit();
        let transaction = self.history.undo.pop()?;
        let mut records = Vec::new();
        // 各逆編集は「その編集を適用する直前」の座標系で記録されている。
        // したがって適用と厳密に逆順 (ステップも、ステップ内の編集も) にたどる必要がある。
        for step in transaction.steps.iter().rev() {
            for edit in step.inverse.iter().rev() {
                let (_, record) = self.apply_one(edit);
                records.push(record);
            }
        }
        let selections = transaction.selections_before.clone();
        self.history.redo.push(transaction);
        self.version += 1;
        Some((selections, records))
    }

    /// 取り消した操作をやり直す。
    pub fn redo(&mut self) -> Option<(Vec<Selection>, Vec<EditRecord>)> {
        let transaction = self.history.redo.pop()?;
        let mut records = Vec::new();
        for step in &transaction.steps {
            for edit in &step.forward {
                let (_, record) = self.apply_one(edit);
                records.push(record);
            }
        }
        let selections = transaction.selections_after.clone();
        self.history.undo.push(transaction);
        self.version += 1;
        Some((selections, records))
    }

    /// 編集列を検証し、文書内で後ろにあるものが先に来るよう並べ替える。
    ///
    /// 後ろから適用すれば、まだ適用していない編集のオフセットが動かないため、
    /// 呼び出し側が座標を補正する必要がなくなる。
    fn validate_and_sort(&self, edits: &[Edit]) -> Result<Vec<Edit>> {
        let len = self.rope.len_chars();
        for edit in edits {
            if edit.range.start > edit.range.end || edit.range.end > len {
                return Err(CoreError::OutOfBounds {
                    offset: edit.range.end,
                    len,
                });
            }
        }
        let mut sorted = edits.to_vec();
        sorted.sort_by(|a, b| b.range.start.cmp(&a.range.start));
        for pair in sorted.windows(2) {
            // 降順に並んでいるので、後続 (文書内で手前) の終端が先行の開始を超えたら重なり。
            if pair[1].range.end > pair[0].range.start {
                return Err(CoreError::OverlappingEdits);
            }
        }
        Ok(sorted)
    }

    /// 編集 1 件を適用し、逆編集と位置情報を返す。
    fn apply_one(&mut self, edit: &Edit) -> (Edit, EditRecord) {
        let TextRange { start, end } = edit.range;
        let start_byte = self.rope.char_to_byte(start);
        let old_end_byte = self.rope.char_to_byte(end);
        let start_point = self.byte_point(start);
        let old_end_point = self.byte_point(end);
        let removed = self.rope.slice(start..end).to_string();

        if start < end {
            self.rope.remove(start..end);
        }
        if !edit.text.is_empty() {
            self.rope.insert(start, &edit.text);
        }

        let new_end = start + edit.text.chars().count();
        let new_end_byte = self.rope.char_to_byte(new_end);
        let new_end_point = self.byte_point(new_end);

        let inverse = Edit {
            range: TextRange::new(start, new_end),
            text: removed,
        };
        let record = EditRecord {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point,
        };
        (inverse, record)
    }

    fn byte_point(&self, char_offset: usize) -> BytePoint {
        let row = self.rope.char_to_line(char_offset);
        let line_start_byte = self.rope.char_to_byte(self.rope.line_to_char(row));
        BytePoint {
            row,
            column: self.rope.char_to_byte(char_offset) - line_start_byte,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_sel() -> Vec<Selection> {
        vec![Selection::caret(0)]
    }

    fn edit(buffer: &mut TextBuffer, edits: &[Edit]) {
        buffer.edit(edits, &no_sel(), &no_sel()).unwrap();
    }

    #[test]
    fn 挿入と削除が反映される() {
        let mut b = TextBuffer::new("hello");
        edit(&mut b, &[Edit::insert(5, " world")]);
        assert_eq!(b.text(), "hello world");
        edit(&mut b, &[Edit::delete(TextRange::new(0, 6))]);
        assert_eq!(b.text(), "world");
    }

    #[test]
    fn 複数編集は後ろから適用され座標がずれない() {
        let mut b = TextBuffer::new("aaa bbb ccc");
        edit(
            &mut b,
            &[
                Edit::replace(TextRange::new(0, 3), "XXXXX"),
                Edit::replace(TextRange::new(8, 11), "Y"),
            ],
        );
        assert_eq!(b.text(), "XXXXX bbb Y");
    }

    #[test]
    fn 重なる編集は拒否される() {
        let mut b = TextBuffer::new("abcdef");
        let result = b.edit(
            &[
                Edit::replace(TextRange::new(0, 4), "X"),
                Edit::replace(TextRange::new(2, 6), "Y"),
            ],
            &no_sel(),
            &no_sel(),
        );
        assert!(matches!(result, Err(CoreError::OverlappingEdits)));
        assert_eq!(b.text(), "abcdef", "拒否時は内容が変わらない");
    }

    #[test]
    fn 範囲外の編集は拒否される() {
        let mut b = TextBuffer::new("abc");
        let result = b.edit(&[Edit::insert(99, "x")], &no_sel(), &no_sel());
        assert!(matches!(result, Err(CoreError::OutOfBounds { .. })));
    }

    #[test]
    fn undo_と_redo_が往復する() {
        let mut b = TextBuffer::new("hello");
        edit(&mut b, &[Edit::insert(5, " world")]);
        b.commit();
        edit(&mut b, &[Edit::insert(11, "!")]);
        b.commit();
        assert_eq!(b.text(), "hello world!");

        b.undo().unwrap();
        assert_eq!(b.text(), "hello world");
        b.undo().unwrap();
        assert_eq!(b.text(), "hello");
        assert!(b.undo().is_none());

        b.redo().unwrap();
        assert_eq!(b.text(), "hello world");
        b.redo().unwrap();
        assert_eq!(b.text(), "hello world!");
        assert!(b.redo().is_none());
    }

    #[test]
    fn 複数ステップが一つの_undo_単位になる() {
        let mut b = TextBuffer::new("abc");
        // 「選択を消して打ち直す」に相当する 2 ステップ。
        edit(&mut b, &[Edit::delete(TextRange::new(0, 3))]);
        edit(&mut b, &[Edit::insert(0, "xyz")]);
        b.commit();
        assert_eq!(b.text(), "xyz");
        b.undo().unwrap();
        assert_eq!(b.text(), "abc", "2 ステップまとめて戻る");
    }

    #[test]
    fn 複数編集を含むトランザクションを_undo_できる() {
        let mut b = TextBuffer::new("aaa bbb ccc");
        edit(
            &mut b,
            &[
                Edit::replace(TextRange::new(0, 3), "XXXXX"),
                Edit::replace(TextRange::new(8, 11), "Y"),
            ],
        );
        b.commit();
        b.undo().unwrap();
        assert_eq!(b.text(), "aaa bbb ccc");
        b.redo().unwrap();
        assert_eq!(b.text(), "XXXXX bbb Y");
    }

    #[test]
    fn 新規編集で_redo_履歴が捨てられる() {
        let mut b = TextBuffer::new("a");
        edit(&mut b, &[Edit::insert(1, "b")]);
        b.commit();
        b.undo().unwrap();
        assert!(b.can_redo());
        edit(&mut b, &[Edit::insert(1, "c")]);
        assert!(!b.can_redo());
        assert_eq!(b.text(), "ac");
    }

    #[test]
    fn 保存済み判定が_undo_で戻る() {
        let mut b = TextBuffer::new("hello");
        assert!(!b.is_dirty());
        edit(&mut b, &[Edit::insert(5, "!")]);
        b.commit();
        assert!(b.is_dirty());
        b.undo().unwrap();
        assert!(!b.is_dirty(), "保存時点の内容に戻れば未変更扱い");
        b.redo().unwrap();
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
    }

    #[test]
    fn 版数不一致の編集は拒否される() {
        let mut b = TextBuffer::new("abc");
        let result = b.apply_versioned(99, &[Edit::insert(0, "x")], &no_sel(), &no_sel());
        assert!(matches!(result, Err(CoreError::VersionMismatch { .. })));
    }

    #[test]
    fn 選択範囲が_undo_で復元される() {
        let mut b = TextBuffer::new("hello");
        let before = vec![Selection::new(0, 5)];
        let after = vec![Selection::caret(1)];
        b.edit(&[Edit::replace(TextRange::new(0, 5), "X")], &before, &after)
            .unwrap();
        b.commit();
        let (restored, _) = b.undo().unwrap();
        assert_eq!(restored, before);
        let (restored, _) = b.redo().unwrap();
        assert_eq!(restored, after);
    }

    #[test]
    fn 編集記録がバイト位置を正しく表す() {
        // 「あ」は UTF-8 で 3 バイト。
        let mut b = TextBuffer::new("あいう\nxyz");
        let records = b
            .edit(&[Edit::insert(1, "X")], &no_sel(), &no_sel())
            .unwrap();
        let r = records[0];
        assert_eq!(r.start_byte, 3);
        assert_eq!(r.old_end_byte, 3);
        assert_eq!(r.new_end_byte, 4);
        assert_eq!(r.start_point, BytePoint { row: 0, column: 3 });
        assert_eq!(r.new_end_point, BytePoint { row: 0, column: 4 });
    }

    #[test]
    fn 改行を跨ぐ削除の記録が正しい() {
        let mut b = TextBuffer::new("ab\ncd\nef");
        let records = b
            .edit(&[Edit::delete(TextRange::new(1, 7))], &no_sel(), &no_sel())
            .unwrap();
        assert_eq!(b.text(), "af");
        let r = records[0];
        assert_eq!(r.start_point, BytePoint { row: 0, column: 1 });
        assert_eq!(r.old_end_point, BytePoint { row: 2, column: 1 });
        assert_eq!(r.new_end_point, BytePoint { row: 0, column: 1 });
    }

    #[test]
    fn 再読込で履歴が破棄される() {
        let mut b = TextBuffer::new("a");
        edit(&mut b, &[Edit::insert(1, "b")]);
        b.commit();
        b.reset("zzz");
        assert_eq!(b.text(), "zzz");
        assert!(!b.can_undo());
        assert!(!b.is_dirty());
    }
}
