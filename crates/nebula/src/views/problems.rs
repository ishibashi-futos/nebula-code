//! 問題 (診断) 一覧。
//!
//! バックエンドは言語サーバーから届いた診断を [`Event::Diagnostics`] で **ファイル単位に
//! 丸ごと** 送ってくる。差分ではないため、同じパスの再通知は置き換え、空で届いたパスは
//! 一覧から取り除く。これが LSP の `publishDiagnostics` の意味論そのものなので、
//! 差分をこちらで組み立てると必ずずれる。
//!
//! 大きなリポジトリでは診断が数千件になりうるので、描画は `uniform_list` で可視行だけに
//! 絞る。そのために「ファイル見出し + 診断行」の入れ子を平坦な行列へ写す
//! ([`flatten_rows`])。並べ替え・グループ化・絞り込みはいずれも gpui に依存しない
//! 純粋関数として切り出し、単体テストで固めてある。
//!
//! このビューは表示に専念する。行をクリックしても飛び先は開かず、選択状態を持つだけ。
//! 選択は [`ProblemsView::selected`] で取れるので、シェル側が拾えるようになった時点で
//! ジャンプに繋げられる。

use crate::assets::Icon;
use crate::theme::{Theme, theme};
use crate::ui::{
    TextInput, TextInputEvent, empty_state, focus_border, h_flex, icon, list_row,
    nebula_accent_line, panel_header, v_flex,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, CursorStyle, Entity, Hsla, MouseButton, Pixels, Subscription, Window, div,
    px, uniform_list,
};
use nebula_protocol::{Diagnostic, DiagnosticSeverity, Event, Position};
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// 1 行に詰め込むメッセージの上限。rustc の長い説明で行が横に伸び続けるのを防ぐ。
const MAX_MESSAGE_CHARS: usize = 240;

/// 一覧の行の高さ。`uniform_list` は全行が同じ高さである前提なので 1 か所で持つ。
const ROW_HEIGHT: Pixels = px(22.);

// ---------------------------------------------------------------------------
// 表示用のデータ整形 (純粋関数)
// ---------------------------------------------------------------------------

/// 1 ファイルぶんの診断。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileGroup {
    path: PathBuf,
    /// 共通の親ディレクトリを取り除いた表示名。
    label: String,
    /// 重要度順・行番号順に並べ、絞り込みを通したもの。
    diagnostics: Vec<Diagnostic>,
    errors: usize,
    warnings: usize,
    collapsed: bool,
}

/// `uniform_list` に渡す平坦な 1 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProblemRow {
    Header { group: usize },
    Item { group: usize, index: usize },
}

/// 診断を重要度順 (Error → Warning → Information → Hint)、同順なら行桁順に並べる。
///
/// 言語サーバーは出現順で送ってくるため、そのまま出すと重大なものが下に埋もれる。
fn sorted_diagnostics(diagnostics: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut sorted = diagnostics.to_vec();
    // `DiagnosticSeverity` の derive した Ord が宣言順 = 重要度順になっている。
    sorted.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(a.range.start.row.cmp(&b.range.start.row))
            .then(a.range.start.column.cmp(&b.range.start.column))
    });
    sorted
}

/// パス単位の全置換。空で届いたパスは一覧から消す。
///
/// バックエンドの診断通知は差分ではなくファイル単位の全量なので、追記ではなく置換する。
fn upsert_diagnostics(
    store: &mut Vec<(PathBuf, Vec<Diagnostic>)>,
    path: PathBuf,
    diagnostics: &[Diagnostic],
) {
    let existing = store.iter().position(|(stored, _)| *stored == path);
    if diagnostics.is_empty() {
        if let Some(index) = existing {
            store.remove(index);
        }
        return;
    }
    let sorted = sorted_diagnostics(diagnostics);
    match existing {
        Some(index) => store[index].1 = sorted,
        None => {
            // パス順に挿入する。到着順のままだと通知のたびに見出しの並びが入れ替わる。
            let at = store.partition_point(|(stored, _)| stored.as_path() < path.as_path());
            store.insert(at, (path, sorted));
        }
    }
}

/// 全パスに共通する親ディレクトリ。
///
/// このビューはワークスペースルートを知らされないため、表示を短くする基準を
/// 手元のパスだけから決める。1 ファイルなら「そのファイルの親」になり、
/// 見出しはファイル名だけになる。
fn common_root<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<PathBuf> {
    let mut common: Option<Vec<std::ffi::OsString>> = None;
    for path in paths {
        let parts: Vec<std::ffi::OsString> = path
            .parent()
            .map(|parent| {
                parent
                    .components()
                    .map(|c| c.as_os_str().to_os_string())
                    .collect()
            })
            .unwrap_or_default();
        common = Some(match common {
            None => parts,
            Some(previous) => previous
                .into_iter()
                .zip(parts)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    let common = common?;
    if common.is_empty() {
        return None;
    }
    Some(common.into_iter().collect())
}

/// 共通の親を取り除いた表示名。
fn relative_label(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// 絞り込みの判定。`needle` は呼び出し側で小文字化済みであること。
fn matches_filter(diagnostic: &Diagnostic, label: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    diagnostic.message.to_lowercase().contains(needle) || label.to_lowercase().contains(needle)
}

/// 保持している診断を、表示用のファイル別グループへ畳む。
fn build_groups(
    store: &[(PathBuf, Vec<Diagnostic>)],
    root: Option<&Path>,
    query: &str,
    collapsed: &HashSet<PathBuf>,
) -> Vec<FileGroup> {
    let needle = query.trim().to_lowercase();
    store
        .iter()
        .filter_map(|(path, diagnostics)| {
            let label = relative_label(path, root);
            let kept: Vec<Diagnostic> = diagnostics
                .iter()
                .filter(|d| matches_filter(d, &label, &needle))
                .cloned()
                .collect();
            if kept.is_empty() {
                return None;
            }
            let errors = kept
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::Error)
                .count();
            let warnings = kept
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::Warning)
                .count();
            Some(FileGroup {
                path: path.clone(),
                label,
                diagnostics: kept,
                errors,
                warnings,
                collapsed: collapsed.contains(path),
            })
        })
        .collect()
}

/// 入れ子のグループを、見出しと診断行が交互に並ぶ 1 次元の列へ写す。
///
/// `uniform_list` は「添字 → 1 行」しか扱えないため、この平坦化が要る。
fn flatten_rows(groups: &[FileGroup]) -> Vec<ProblemRow> {
    let mut rows = Vec::new();
    for (group, entry) in groups.iter().enumerate() {
        rows.push(ProblemRow::Header { group });
        if entry.collapsed {
            continue;
        }
        for index in 0..entry.diagnostics.len() {
            rows.push(ProblemRow::Item { group, index });
        }
    }
    rows
}

/// 保持している全診断のエラー数と警告数。ヘッダのバッジに出す。
fn total_counts(store: &[(PathBuf, Vec<Diagnostic>)]) -> (usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    for (_, diagnostics) in store {
        for diagnostic in diagnostics {
            match diagnostic.severity {
                DiagnosticSeverity::Error => errors += 1,
                DiagnosticSeverity::Warning => warnings += 1,
                _ => {}
            }
        }
    }
    (errors, warnings)
}

/// 表示名をディレクトリ部とファイル名に割る。ファイル名だけを強く出すため。
fn split_label(label: &str) -> (&str, &str) {
    match label.rfind('/') {
        Some(index) => (&label[..index], &label[index + 1..]),
        None => ("", label),
    }
}

/// 複数行のメッセージを 1 行に潰す。
///
/// rustc の診断は改行と連続空白を含む。そのまま行に流すと高さが崩れ、
/// `uniform_list` の「全行同じ高さ」という前提が壊れる。
fn one_line(message: &str) -> String {
    let flattened: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_MESSAGE_CHARS {
        return flattened;
    }
    let mut truncated: String = flattened.chars().take(MAX_MESSAGE_CHARS).collect();
    truncated.push('…');
    truncated
}

/// `行:桁` の表示。プロトコルの行桁は 0 始まりなので 1 足す。
fn position_label(position: Position) -> String {
    format!("{}:{}", position.row + 1, position.column + 1)
}

fn severity_icon(severity: DiagnosticSeverity) -> Icon {
    match severity {
        DiagnosticSeverity::Error => Icon::Error,
        DiagnosticSeverity::Warning => Icon::Warning,
        DiagnosticSeverity::Information | DiagnosticSeverity::Hint => Icon::Problems,
    }
}

// ---------------------------------------------------------------------------
// ビュー
// ---------------------------------------------------------------------------

/// 選択中の問題。
///
/// 行番号ではなく中身で覚える。絞り込みや再通知で行の並びが変わっても、
/// 同じ診断を指し続けられるようにするため。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProblem {
    pub path: PathBuf,
    pub position: Position,
    pub message: String,
}

pub struct ProblemsView {
    /// パス順に保つ診断の保管庫。各パスの中身は重要度順に並べてある。
    diagnostics: Vec<(PathBuf, Vec<Diagnostic>)>,
    filter: Entity<TextInput>,
    query: String,
    collapsed: HashSet<PathBuf>,
    groups: Vec<FileGroup>,
    rows: Vec<ProblemRow>,
    selected: Option<SelectedProblem>,
    /// 保持しないと購読が即座に解除される。
    _subscriptions: Vec<Subscription>,
}

impl ProblemsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::single_line("問題を絞り込む (メッセージ・パス)", cx));
        let subscription = cx.subscribe(&filter, |this, input, event, cx| match event {
            TextInputEvent::Changed => {
                this.query = input.read(cx).text().to_string();
                this.rebuild();
                cx.notify();
            }
            // 枠のネオンはこのビューが描くので、出入りのたびに描き直す。
            TextInputEvent::FocusChanged => cx.notify(),
            _ => {}
        });
        Self {
            diagnostics: Vec::new(),
            filter,
            query: String::new(),
            collapsed: HashSet::new(),
            groups: Vec::new(),
            rows: Vec::new(),
            selected: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        // 診断以外の通知でも再描画すると、ターミナル出力のたびにこのパネルが再描画される。
        let Event::Diagnostics { path, diagnostics } = event else {
            return;
        };
        upsert_diagnostics(&mut self.diagnostics, path.clone(), diagnostics);
        self.rebuild();
        cx.notify();
    }

    /// 選択中の問題。将来シェル側がジャンプに使えるよう公開している。
    ///
    /// このビューはまだ出来事を上げる手段を持たないため、現時点では呼び出し元がいない。
    #[allow(dead_code)]
    pub fn selected(&self) -> Option<&SelectedProblem> {
        self.selected.as_ref()
    }

    /// 保管庫から表示用のグループと行を組み直す。
    ///
    /// 描画のたびに走らせると絞り込みのたびに全件を舐めることになるので、
    /// 診断の到着と絞り込みの変更のときだけ呼ぶ。
    fn rebuild(&mut self) {
        let root = common_root(self.diagnostics.iter().map(|(path, _)| path.as_path()));
        self.groups = build_groups(
            &self.diagnostics,
            root.as_deref(),
            &self.query,
            &self.collapsed,
        );
        self.rows = flatten_rows(&self.groups);
        // 解決済みの診断を選択したままにしない。絞り込みで隠れただけの場合は残す。
        if let Some(selected) = &self.selected {
            let alive = self.diagnostics.iter().any(|(path, diagnostics)| {
                path == &selected.path
                    && diagnostics.iter().any(|d| {
                        d.range.start == selected.position && d.message == selected.message
                    })
            });
            if !alive {
                self.selected = None;
            }
        }
    }

    fn is_selected(&self, path: &Path, diagnostic: &Diagnostic) -> bool {
        self.selected.as_ref().is_some_and(|selected| {
            selected.path == path
                && selected.position == diagnostic.range.start
                && selected.message == diagnostic.message
        })
    }

    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (errors, warnings) = total_counts(&self.diagnostics);
        panel_header("問題", cx)
            .flex_none()
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(count_badge(Icon::Error, errors, theme.error, theme))
                    .child(count_badge(Icon::Warning, warnings, theme.warning, theme)),
            )
            .into_any_element()
    }

    fn render_filter(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.filter.read(cx).is_focused();
        div()
            .flex_none()
            .px(px(10.))
            .pb(px(6.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .w_full()
                    .h(px(24.))
                    .px(px(7.))
                    .gap(px(6.))
                    .overflow_hidden()
                    .rounded(px(5.))
                    .bg(theme.bg_surface)
                    .border_1()
                    // フォーカス中はネオンで縁取る。どの欄を打っているか一目で分かる。
                    .border_color(focus_border(focused, theme))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(theme.text)
                    .cursor(CursorStyle::IBeam)
                    // 余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = self.filter.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(icon(Icon::Search, px(11.), theme.text_faint))
                    .child(div().w_full().child(self.filter.clone())),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.diagnostics.is_empty() {
            return empty_state("問題は検出されていません", cx).into_any_element();
        }
        if self.rows.is_empty() {
            return empty_state("絞り込みに一致する問題はありません", cx).into_any_element();
        }

        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "problems-rows",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        // テーマは行ごとに引かず、可視範囲ぶんで 1 回だけ複製する。
                        let theme = theme(cx).clone();
                        range
                            .filter_map(|row| {
                                Some(match *this.rows.get(row)? {
                                    ProblemRow::Header { group } => {
                                        this.render_group_header(row, group, &theme, cx)
                                    }
                                    ProblemRow::Item { group, index } => {
                                        this.render_item(row, group, index, &theme, cx)
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h_full(),
            )
            .into_any_element()
    }

    fn render_group_header(
        &self,
        row: usize,
        group: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.groups.get(group) else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        let (dir, name) = split_label(&entry.label);
        let path = entry.path.clone();
        let collapsed = entry.collapsed;

        list_row(("problems-row", row), false, cx)
            .h(ROW_HEIGHT)
            .bg(theme.bg_surface)
            .child(icon(
                if collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronDown
                },
                px(11.),
                theme.text_faint,
            ))
            .child(icon(Icon::File, px(12.), theme.accent_tertiary))
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(dir.to_string()),
            )
            .when(entry.errors > 0, |el| {
                el.child(count_badge(Icon::Error, entry.errors, theme.error, theme))
            })
            .when(entry.warnings > 0, |el| {
                el.child(count_badge(
                    Icon::Warning,
                    entry.warnings,
                    theme.warning,
                    theme,
                ))
            })
            .on_click(cx.listener(move |this, _, _window, cx| {
                if !this.collapsed.remove(&path) {
                    this.collapsed.insert(path.clone());
                }
                this.rebuild();
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_item(
        &self,
        row: usize,
        group: usize,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.groups.get(group) else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        let Some(diagnostic) = entry.diagnostics.get(index) else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        let color = theme.diagnostic_color(diagnostic.severity);
        let selected = self.is_selected(&entry.path, diagnostic);
        let picked = SelectedProblem {
            path: entry.path.clone(),
            position: diagnostic.range.start,
            message: diagnostic.message.clone(),
        };
        // source と code は「rustc(E0308)」のように 1 つにまとめる。右端は狭い。
        let origin = match (&diagnostic.source, &diagnostic.code) {
            (Some(source), Some(code)) => format!("{source}({code})"),
            (Some(source), None) => source.clone(),
            (None, Some(code)) => code.clone(),
            (None, None) => String::new(),
        };

        list_row(("problems-row", row), selected, cx)
            .relative()
            .h(ROW_HEIGHT)
            .pl(px(26.))
            // 選択行の左端にネオンの縦線を引く。背景の淡い色だけでは暗所で見分けにくい。
            .when(selected, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(3.))
                        .bottom(px(3.))
                        .w(px(2.))
                        .rounded_r(px(2.))
                        .bg(theme.accent),
                )
            })
            .child(icon(severity_icon(diagnostic.severity), px(12.), color))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme.text)
                    .child(one_line(&diagnostic.message)),
            )
            .when(!origin.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(origin),
                )
            })
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(position_label(diagnostic.range.start)),
            )
            .on_click(cx.listener(move |this, _, _window, cx| {
                // 開く先へ飛ばす手段をまだ持たないため、選択だけを覚える。
                this.selected = Some(picked.clone());
                cx.notify();
            }))
            .into_any_element()
    }
}

/// 件数のバッジ。0 件でも幅を保って出し、増減で見出しが揺れないようにする。
fn count_badge(glyph: Icon, count: usize, color: Hsla, theme: &Theme) -> AnyElement {
    let dimmed = count == 0;
    h_flex()
        .gap(px(3.))
        .px(px(5.))
        .h(px(16.))
        .rounded(px(8.))
        .bg(if dimmed {
            theme.bg_surface
        } else {
            theme.accent_soft
        })
        .child(icon(
            glyph,
            px(10.),
            if dimmed { theme.text_faint } else { color },
        ))
        .child(
            div()
                .text_size(px(10.5))
                .text_color(if dimmed { theme.text_faint } else { color })
                .child(count.to_string()),
        )
        .into_any_element()
}

impl Render for ProblemsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        v_flex()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_elevated)
            // 上端の 1px。星雲の縁のような淡い発光でパネルの境界を示す。
            .child(nebula_accent_line(theme.border_glow))
            .child(self.render_header(&theme, cx))
            .child(self.render_filter(&theme, cx))
            .child(self.render_body(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::SpanRange;

    fn 診断(severity: DiagnosticSeverity, row: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: SpanRange::new(Position::new(row, 0), Position::new(row, 1)),
            severity,
            message: message.to_string(),
            source: Some("rustc".into()),
            code: None,
        }
    }

    #[test]
    fn 重要度の順序が宣言順になっている() {
        // 並べ替えは derive した Ord に頼っている。プロトコル側で順序が入れ替わると
        // 表示順が静かに壊れるため、ここで固定する。
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Information);
        assert!(DiagnosticSeverity::Information < DiagnosticSeverity::Hint);
    }

    #[test]
    fn 重要度順そのあと行番号順に並ぶ() {
        let input = vec![
            診断(DiagnosticSeverity::Warning, 3, "w3"),
            診断(DiagnosticSeverity::Error, 9, "e9"),
            診断(DiagnosticSeverity::Hint, 0, "h0"),
            診断(DiagnosticSeverity::Error, 2, "e2"),
            診断(DiagnosticSeverity::Information, 1, "i1"),
        ];
        let sorted = sorted_diagnostics(&input);
        let messages: Vec<&str> = sorted.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, vec!["e2", "e9", "w3", "i1", "h0"]);
    }

    #[test]
    fn 同じパスの再通知は置き換える() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[診断(DiagnosticSeverity::Error, 0, "古い")],
        );
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[
                診断(DiagnosticSeverity::Error, 0, "新しい1"),
                診断(DiagnosticSeverity::Error, 1, "新しい2"),
            ],
        );
        assert_eq!(store.len(), 1, "パスが増えている");
        assert_eq!(store[0].1.len(), 2);
        assert_eq!(store[0].1[0].message, "新しい1");
    }

    #[test]
    fn 空の診断でパスが消える() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[診断(DiagnosticSeverity::Error, 0, "e")],
        );
        upsert_diagnostics(&mut store, PathBuf::from("/w/a.rs"), &[]);
        assert!(store.is_empty());
    }

    #[test]
    fn 未知のパスに空の診断が来ても壊れない() {
        let mut store = Vec::new();
        upsert_diagnostics(&mut store, PathBuf::from("/w/never.rs"), &[]);
        assert!(store.is_empty());
    }

    #[test]
    fn パスは辞書順に保たれる() {
        let mut store = Vec::new();
        for name in ["/w/c.rs", "/w/a.rs", "/w/b.rs"] {
            upsert_diagnostics(
                &mut store,
                PathBuf::from(name),
                &[診断(DiagnosticSeverity::Error, 0, "e")],
            );
        }
        let paths: Vec<&str> = store.iter().filter_map(|(p, _)| p.to_str()).collect();
        assert_eq!(paths, vec!["/w/a.rs", "/w/b.rs", "/w/c.rs"]);
    }

    #[test]
    fn 単一ファイルなら見出しはファイル名だけになる() {
        let path = PathBuf::from("/w/src/main.rs");
        let root = common_root([path.as_path()]);
        assert_eq!(relative_label(&path, root.as_deref()), "main.rs");
    }

    #[test]
    fn 共通の親ディレクトリを取り除く() {
        let a = PathBuf::from("/w/src/app/main.rs");
        let b = PathBuf::from("/w/src/lib/util.rs");
        let root = common_root([a.as_path(), b.as_path()]).expect("共通の親がある");
        assert_eq!(root, PathBuf::from("/w/src"));
        assert_eq!(relative_label(&a, Some(&root)), "app/main.rs");
        assert_eq!(relative_label(&b, Some(&root)), "lib/util.rs");
    }

    #[test]
    fn 共通の親が無ければ絶対パスのまま出す() {
        let a = PathBuf::from("/x/a.rs");
        let b = PathBuf::from("y/b.rs");
        assert!(common_root([a.as_path(), b.as_path()]).is_none());
        assert_eq!(relative_label(&a, None), "/x/a.rs");
    }

    #[test]
    fn 絞り込みは大文字小文字を無視してメッセージとパスに効く() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/src/parser.rs"),
            &[診断(DiagnosticSeverity::Error, 0, "Unexpected token")],
        );
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/src/theme.rs"),
            &[診断(DiagnosticSeverity::Warning, 0, "unused import")],
        );
        let root = common_root(store.iter().map(|(p, _)| p.as_path()));
        let collapsed = HashSet::new();

        let by_message = build_groups(&store, root.as_deref(), "UNEXPECTED", &collapsed);
        assert_eq!(by_message.len(), 1);
        assert_eq!(by_message[0].label, "parser.rs");

        let by_path = build_groups(&store, root.as_deref(), "theme", &collapsed);
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].label, "theme.rs");

        let nothing = build_groups(&store, root.as_deref(), "該当なし", &collapsed);
        assert!(nothing.is_empty());
    }

    #[test]
    fn グループごとにエラー数と警告数を数える() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[
                診断(DiagnosticSeverity::Error, 0, "e1"),
                診断(DiagnosticSeverity::Error, 1, "e2"),
                診断(DiagnosticSeverity::Warning, 2, "w1"),
                診断(DiagnosticSeverity::Hint, 3, "h1"),
            ],
        );
        let groups = build_groups(&store, None, "", &HashSet::new());
        assert_eq!(groups[0].errors, 2);
        assert_eq!(groups[0].warnings, 1);
        assert_eq!(total_counts(&store), (2, 1));
    }

    #[test]
    fn 平坦化は見出しと診断行を交互に並べる() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[
                診断(DiagnosticSeverity::Error, 0, "e1"),
                診断(DiagnosticSeverity::Error, 1, "e2"),
            ],
        );
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/b.rs"),
            &[診断(DiagnosticSeverity::Warning, 0, "w1")],
        );
        let groups = build_groups(&store, None, "", &HashSet::new());
        let rows = flatten_rows(&groups);
        assert_eq!(
            rows,
            vec![
                ProblemRow::Header { group: 0 },
                ProblemRow::Item { group: 0, index: 0 },
                ProblemRow::Item { group: 0, index: 1 },
                ProblemRow::Header { group: 1 },
                ProblemRow::Item { group: 1, index: 0 },
            ]
        );
    }

    #[test]
    fn 折りたたんだ見出しは診断行を出さない() {
        let mut store = Vec::new();
        upsert_diagnostics(
            &mut store,
            PathBuf::from("/w/a.rs"),
            &[診断(DiagnosticSeverity::Error, 0, "e1")],
        );
        let collapsed = HashSet::from([PathBuf::from("/w/a.rs")]);
        let groups = build_groups(&store, None, "", &collapsed);
        assert_eq!(flatten_rows(&groups), vec![ProblemRow::Header { group: 0 }]);
    }

    #[test]
    fn 表示名をディレクトリとファイル名に割る() {
        assert_eq!(split_label("app/main.rs"), ("app", "main.rs"));
        assert_eq!(split_label("main.rs"), ("", "main.rs"));
    }

    #[test]
    fn 複数行のメッセージが一行になる() {
        assert_eq!(one_line("型が\n   合いません\n\n"), "型が 合いません");
    }

    #[test]
    fn 長すぎるメッセージは末尾を省く() {
        let long = "あ".repeat(MAX_MESSAGE_CHARS + 50);
        let shortened = one_line(&long);
        assert_eq!(shortened.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(shortened.ends_with('…'));
    }

    #[test]
    fn 行桁の表示は一始まり() {
        assert_eq!(position_label(Position::new(0, 0)), "1:1");
        assert_eq!(position_label(Position::new(41, 7)), "42:8");
    }
}
