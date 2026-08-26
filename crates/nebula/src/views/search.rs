//! ワークスペース全文検索 (ripgrep)。
//!
//! バックエンドは `StartSearch` に対して即座に [`SearchId`] を返し、以降の一致は
//! [`Event::SearchMatches`] で少しずつ流れてくる。全件を待ってから描くと、大きな
//! リポジトリでは数秒間なにも出ない画面になるため、届いたそばから追記する。
//!
//! 結果は数千件になりうるので、描画は `uniform_list` で可視行だけに絞る。そのために
//! 「ファイル見出し + 一致行」の入れ子構造を平坦な行列へ写す ([`flatten_rows`])。
//! この変換と行の整形は gpui に依存しない純粋関数として切り出し、単体テストで固める。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{Theme, theme};
use crate::ui::{
    TextInput, TextInputEvent, empty_state, focus_border, ghost_button, h_flex, icon, icon_button,
    list_row, nebula_accent_line, panel_header, primary_button, simple_tooltip, tooltip_text,
    v_flex,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, CursorStyle, Div, ElementId, Entity, EventEmitter, HighlightStyle,
    MouseButton, Stateful, StyledText, Subscription, Task, Window, div, px, uniform_list,
};
use nebula_protocol::{
    Event, Position, ProtocolError, ProtocolErrorKind, Request, Response, SearchId, SearchMatch,
    SearchQuery, TextRange, WorkspaceInfo,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 入力が止まってから検索を始めるまでの待ち時間。
///
/// 打鍵ごとに rg を起動すると、1 文字目の検索がワークスペース全体を走査してしまう。
const DEBOUNCE: Duration = Duration::from_millis(200);

/// 一致行に表示する最大文字数。桁の多い最小化 JS などで行全体を整形しないための上限。
const MAX_LINE_CHARS: usize = 200;

/// 長い行を切るとき、一致箇所の手前に残す文字数。
const LEAD_CHARS: usize = 16;

/// シェルへ伝える出来事。
pub enum SearchEvent {
    OpenLocation { path: PathBuf, position: Position },
}

/// 入力欄を検索パネルの見た目の箱に収める。
///
/// [`TextInput`] は枠も余白も持たないので、4 つの欄で共通の装いをここで着せる。
fn input_box(input: &Entity<TextInput>, theme: &Theme, cx: &App) -> Div {
    let focused = input.read(cx).is_focused();
    div()
        .w_full()
        .h(px(26.))
        .px(px(7.))
        .flex()
        .items_center()
        .overflow_hidden()
        .rounded(px(5.))
        .bg(theme.bg_surface)
        .border_1()
        // フォーカス中はネオンで縁取る。どの欄を打っているか一目で分かるようにする。
        .border_color(focus_border(focused, theme))
        .text_size(px(12.))
        .line_height(px(17.))
        .text_color(theme.text)
        .cursor(CursorStyle::IBeam)
        // 余白を押しても欄へ入れるようにする。
        .on_mouse_down(MouseButton::Left, {
            let input = input.clone();
            move |_, window, cx| input.read(cx).focus(window)
        })
        .child(div().w_full().child(input.clone()))
}

// ---------------------------------------------------------------------------
// 表示用のデータ整形 (純粋関数)
// ---------------------------------------------------------------------------

/// 1 ファイルぶんの一致。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileGroup {
    path: PathBuf,
    /// ワークスペースルートからの相対表示。
    relative: String,
    matches: Vec<SearchMatch>,
    collapsed: bool,
}

/// `uniform_list` に渡す平坦な 1 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchRow {
    Header { group: usize },
    Match { group: usize, index: usize },
}

/// 入れ子のファイル別一致を、行の並びへ写す。
///
/// `uniform_list` は「添字 → 1 行」しか扱えないため、見出しと一致行を混ぜた
/// 1 本の列にしておく必要がある。
fn flatten_rows(groups: &[FileGroup]) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    for (group, entry) in groups.iter().enumerate() {
        rows.push(SearchRow::Header { group });
        if entry.collapsed {
            continue;
        }
        for index in 0..entry.matches.len() {
            rows.push(SearchRow::Match { group, index });
        }
    }
    rows
}

/// 増分で届いた一致を、ファイルごとの塊へ取り込む。
fn merge_matches(groups: &mut Vec<FileGroup>, root: &Path, incoming: Vec<SearchMatch>) {
    for entry in incoming {
        // rg はファイル単位で順に出すので、末尾を先に見れば大抵 1 回で当たる。
        if groups.last().is_some_and(|g| g.path == entry.path) {
            if let Some(group) = groups.last_mut() {
                group.matches.push(entry);
            }
            continue;
        }
        if let Some(group) = groups.iter_mut().find(|g| g.path == entry.path) {
            group.matches.push(entry);
            continue;
        }
        let path = entry.path.clone();
        let relative = relative_display(root, &path);
        groups.push(FileGroup {
            path,
            relative,
            matches: vec![entry],
            collapsed: false,
        });
    }
}

/// ワークスペースルートからの相対表示。
fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// 相対パスを「親ディレクトリ」「ファイル名」に分ける。
fn split_path(relative: &str) -> (&str, &str) {
    match relative.rfind('/') {
        Some(index) => (&relative[..index], &relative[index + 1..]),
        None => ("", relative),
    }
}

/// カンマ区切りの glob を配列にする。
fn parse_globs(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|glob| !glob.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 一致行を開いたときに飛ぶ位置。
///
/// `line_number` は 1 始まり、[`Position::row`] は 0 始まり。
fn match_position(entry: &SearchMatch) -> Position {
    Position::new(
        entry.line_number.saturating_sub(1),
        entry.matches.first().map(|r| r.start as u32).unwrap_or(0),
    )
}

/// 一致範囲を整える。範囲外を切り、空を捨て、重なりを畳む。
///
/// 重なったまま [`StyledText::with_highlights`] に渡すと、走査が単調でなくなり
/// 文字数の勘定が壊れる。
fn normalize_ranges(ranges: &[TextRange], len: usize) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| r.start.min(len)..r.end.min(len))
        .filter(|r| r.start < r.end)
        .collect();
    spans.sort_by_key(|r| r.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// 一致行を表示用に整える。
///
/// - 行頭のインデントは落とす。深い階層のコードで一致箇所が右へ押し出されるため。
/// - 長すぎる行は一致箇所が入る窓だけを切り出す。
///
/// 返す範囲は **表示文字列内のバイト範囲**。`SearchMatch.matches` は文字オフセットだが、
/// gpui の [`StyledText`] はバイトで受け取るので、ここで変換しきる。
/// 素朴に `&line_text[range]` とすると日本語を含む行で境界を割って落ちる。
fn prepare_match_line(
    line_text: &str,
    matches: &[TextRange],
    max_chars: usize,
) -> (String, Vec<Range<usize>>) {
    let chars: Vec<char> = line_text.chars().collect();
    let spans = normalize_ranges(matches, chars.len());
    let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
    let first = spans.first().map(|r| r.start).unwrap_or(indent);

    // 一致箇所が窓の外へ出ないよう、必要なら手前を捨てる。
    let start = if first > indent + LEAD_CHARS {
        first - LEAD_CHARS
    } else {
        indent.min(chars.len())
    };
    let end = start.saturating_add(max_chars).min(chars.len());

    let mut display = String::new();
    if start > indent {
        display.push('…');
    }
    let prefix_bytes = display.len();

    // 表示部分の「文字位置 → バイト位置」表。範囲変換に使う。
    let mut byte_of_char = Vec::with_capacity(end - start + 1);
    let mut offset = 0usize;
    for ch in &chars[start..end] {
        byte_of_char.push(offset);
        offset += ch.len_utf8();
        display.push(*ch);
    }
    byte_of_char.push(offset);

    let ranges = spans
        .iter()
        .filter_map(|span| {
            let from = span.start.clamp(start, end);
            let to = span.end.clamp(start, end);
            (from < to).then(|| {
                prefix_bytes + byte_of_char[from - start]..prefix_bytes + byte_of_char[to - start]
            })
        })
        .collect();

    if end < chars.len() {
        display.push('…');
    }
    (display, ranges)
}

// ---------------------------------------------------------------------------
// ビュー本体
// ---------------------------------------------------------------------------

/// 検索の進み具合。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchStatus {
    /// まだ検索していない。
    Idle,
    Running,
    Finished {
        total: usize,
        truncated: bool,
    },
    /// ripgrep が無い環境。
    Unsupported,
    Failed(String),
}

/// 開始応答より先に届いた結果。
///
/// `SearchStarted` の応答と `SearchMatches` のイベントは別経路で届くため、
/// 順序が入れ替わることがある。ID が確定するまで貯めて、確定後に選り分ける。
enum StagedEvent {
    Matches(SearchId, Vec<SearchMatch>),
    Finished(SearchId, usize, bool),
}

impl StagedEvent {
    fn search(&self) -> SearchId {
        match self {
            Self::Matches(id, _) | Self::Finished(id, _, _) => *id,
        }
    }
}

pub struct SearchView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,

    query_input: Entity<TextInput>,
    replace_input: Entity<TextInput>,
    include_input: Entity<TextInput>,
    exclude_input: Entity<TextInput>,

    case_sensitive: bool,
    whole_word: bool,
    is_regex: bool,
    show_replace: bool,
    show_filters: bool,

    active_search: Option<SearchId>,
    staged: Vec<StagedEvent>,
    groups: Vec<FileGroup>,
    rows: Vec<SearchRow>,
    status: SearchStatus,
    /// 置換結果などの一時的な案内。
    notice: Option<String>,
    /// 初回描画で検索欄へフォーカスを移したか。
    focus_placed: bool,

    /// デバウンス用。落とすと待機が取り消される。
    _debounce: Option<Task<()>>,
    /// 入力欄の購読。保持しないと即座に解除される。
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchEvent> for SearchView {}

impl SearchView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| TextInput::single_line("検索", cx));
        let replace_input = cx.new(|cx| TextInput::single_line("置換", cx));
        let include_input = cx.new(|cx| TextInput::single_line("例: src/**/*.rs", cx));
        let exclude_input = cx.new(|cx| TextInput::single_line("例: target, *.lock", cx));

        let subscriptions = vec![
            cx.subscribe(&query_input, Self::on_condition_changed),
            cx.subscribe(&include_input, Self::on_condition_changed),
            cx.subscribe(&exclude_input, Self::on_condition_changed),
            cx.subscribe(&replace_input, Self::on_replace_input),
        ];

        Self {
            client: None,
            workspace: None,
            query_input,
            replace_input,
            include_input,
            exclude_input,
            case_sensitive: false,
            whole_word: false,
            is_regex: false,
            show_replace: false,
            show_filters: false,
            active_search: None,
            staged: Vec::new(),
            groups: Vec::new(),
            rows: Vec::new(),
            status: SearchStatus::Idle,
            notice: None,
            focus_placed: false,
            _debounce: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        let changed = self.workspace.as_ref().map(|w| w.id) != Some(workspace.id);
        self.workspace = Some(workspace);
        if changed {
            // 別のフォルダの結果を残すとパスの意味が変わってしまう。
            self.cancel_active(cx);
            self.clear_results();
            self.status = SearchStatus::Idle;
        }
        cx.notify();
    }

    /// バックエンドからの検索イベントを取り込む。
    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        match event {
            Event::SearchMatches { search, matches } => {
                self.ingest(StagedEvent::Matches(*search, matches.clone()), cx);
            }
            Event::SearchFinished {
                search,
                total,
                truncated,
            } => {
                self.ingest(StagedEvent::Finished(*search, *total, *truncated), cx);
            }
            _ => {}
        }
    }

    fn ingest(&mut self, staged: StagedEvent, cx: &mut Context<Self>) {
        match self.active_search {
            Some(active) if staged.search() == active => {
                self.apply(staged);
                cx.notify();
            }
            // ID が未確定の間に届いたものは貯めておく。確定後に選り分ける。
            None if self.status == SearchStatus::Running => self.staged.push(staged),
            // 取り消した検索の残りは捨てる。
            _ => {}
        }
    }

    fn apply(&mut self, staged: StagedEvent) {
        let root = self
            .workspace
            .as_ref()
            .map(|w| w.root.clone())
            .unwrap_or_default();
        match staged {
            StagedEvent::Matches(_, matches) => {
                merge_matches(&mut self.groups, &root, matches);
                self.rows = flatten_rows(&self.groups);
            }
            StagedEvent::Finished(_, total, truncated) => {
                self.status = SearchStatus::Finished { total, truncated };
            }
        }
    }

    // -- 検索の開始と取り消し --

    fn on_condition_changed(
        &mut self,
        _entity: Entity<TextInput>,
        event: &TextInputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TextInputEvent::Changed => self.schedule_search(cx),
            TextInputEvent::Submit => self.start_search(cx),
            // 枠のネオンはこのビューが描くので、出入りのたびに描き直す。
            TextInputEvent::FocusChanged => cx.notify(),
            TextInputEvent::Cancel => {}
        }
    }

    fn on_replace_input(
        &mut self,
        _entity: Entity<TextInput>,
        event: &TextInputEvent,
        cx: &mut Context<Self>,
    ) {
        // 置換語は検索条件ではないので、変わっても検索し直さない。
        match event {
            TextInputEvent::Submit => self.replace_all(cx),
            // 枠のネオンはこのビューが描くので、出入りのたびに描き直す。
            TextInputEvent::FocusChanged => cx.notify(),
            _ => {}
        }
    }

    /// 入力が止まってから検索する。
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| this.start_search(cx)).ok();
        }));
    }

    fn start_search(&mut self, cx: &mut Context<Self>) {
        // 待機中のデバウンスを落とす。Enter との二重起動を防ぐ。
        self._debounce = None;
        self.notice = None;
        self.cancel_active(cx);
        self.clear_results();

        let pattern = self.query_input.read(cx).text().to_string();
        if pattern.is_empty() {
            // 空文字は rg にとって「全行に一致」なので、送ってはいけない。
            self.status = SearchStatus::Idle;
            cx.notify();
            return;
        }
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.clone()) else {
            self.status = SearchStatus::Failed("フォルダが開かれていません".into());
            cx.notify();
            return;
        };

        let query = self.build_query(pattern, cx);
        self.status = SearchStatus::Running;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::StartSearch {
                    workspace: workspace.id,
                    query,
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::SearchStarted { search }) => {
                        this.active_search = Some(search);
                        // 先に届いていた結果を、この検索のぶんだけ取り込む。
                        for staged in std::mem::take(&mut this.staged) {
                            if staged.search() == search {
                                this.apply(staged);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => this.set_error(&e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 進行中の検索を止める。結果を捨てる前に必ず呼ぶ。
    fn cancel_active(&mut self, cx: &mut Context<Self>) {
        self.staged.clear();
        let Some(search) = self.active_search.take() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |_this, _cx| {
            let _ = client.request(Request::CancelSearch { search }).await;
        })
        .detach();
    }

    fn clear_results(&mut self) {
        self.groups.clear();
        self.rows.clear();
    }

    fn build_query(&self, pattern: String, cx: &App) -> SearchQuery {
        SearchQuery {
            pattern,
            is_regex: self.is_regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            include_globs: parse_globs(self.include_input.read(cx).text()),
            exclude_globs: parse_globs(self.exclude_input.read(cx).text()),
            ..SearchQuery::default()
        }
    }

    fn set_error(&mut self, error: &ProtocolError) {
        self.status = match error.kind {
            ProtocolErrorKind::Unsupported => SearchStatus::Unsupported,
            _ => SearchStatus::Failed(error.message.clone()),
        };
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let pattern = self.query_input.read(cx).text().to_string();
        if pattern.is_empty() {
            return;
        }
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.clone()) else {
            return;
        };
        let query = self.build_query(pattern, cx);
        let replacement = self.replace_input.read(cx).text().to_string();
        self.notice = Some("置換しています…".into());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::ReplaceAll {
                    workspace: workspace.id,
                    query,
                    replacement,
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::ReplaceResult {
                        files_changed,
                        replacements,
                    }) => {
                        // 置換後の一致位置は古いので取り直す。案内は取り直しの後に置く。
                        this.start_search(cx);
                        this.notice = Some(format!(
                            "{files_changed} ファイルの {replacements} 箇所を置換しました"
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        this.notice = None;
                        this.set_error(&e);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- 操作 --

    fn toggle_group(&mut self, group: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.groups.get_mut(group) {
            entry.collapsed = !entry.collapsed;
        }
        self.rows = flatten_rows(&self.groups);
        cx.notify();
    }

    fn open_match(&mut self, group: usize, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.groups.get(group).and_then(|g| g.matches.get(index)) else {
            return;
        };
        cx.emit(SearchEvent::OpenLocation {
            path: entry.path.clone(),
            position: match_position(entry),
        });
    }

    fn total_matches(&self) -> usize {
        self.groups.iter().map(|g| g.matches.len()).sum()
    }

    // -- 描画 --

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let summary = match &self.status {
            // 進行中も届いたぶんの件数を出す。増えていくこと自体が進捗の表示になる。
            SearchStatus::Running => {
                Some((format!("検索中… {} 件", self.total_matches()), theme.accent))
            }
            SearchStatus::Finished { total, truncated } => {
                let text = if *truncated {
                    format!("{total} 件以上 / {} ファイル", self.groups.len())
                } else {
                    format!("{total} 件 / {} ファイル", self.groups.len())
                };
                Some((text, theme.text_muted))
            }
            _ => None,
        };

        panel_header("検索", cx)
            .child(
                h_flex()
                    .gap(px(6.))
                    .children(summary.map(|(text, color)| {
                        div().text_size(px(10.5)).text_color(color).child(text)
                    }))
                    .child(
                        icon_button("search-refresh", Icon::Refresh, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.start_search(cx)))
                            .tooltip(simple_tooltip(tooltip_text::SEARCH_REFRESH)),
                    ),
            )
            .into_any_element()
    }

    /// 検索条件の入力部。
    fn render_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let has_pattern = !self.query_input.read(cx).text().is_empty();
        let show_replace = self.show_replace;

        v_flex()
            .px(px(10.))
            .pb(px(8.))
            .gap(px(6.))
            .flex_none()
            .child(
                h_flex()
                    .gap(px(4.))
                    .items_start()
                    .child(
                        // 置換欄の開閉。VS Code と同じく検索欄の左に置く。
                        h_flex()
                            .id("search-toggle-replace")
                            .justify_center()
                            .size(px(26.))
                            .flex_none()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(icon(
                                if show_replace {
                                    Icon::ChevronDown
                                } else {
                                    Icon::ChevronRight
                                },
                                px(13.),
                                theme.text_faint,
                            ))
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.show_replace = !this.show_replace;
                                cx.notify();
                            }))
                            .tooltip(simple_tooltip(tooltip_text::SEARCH_TOGGLE_REPLACE)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(6.))
                            .child(input_box(&self.query_input, &theme, cx))
                            .when(show_replace, |el| {
                                el.child(input_box(&self.replace_input, &theme, cx))
                            }),
                    )
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .flex_none()
                            .child(
                                h_flex()
                                    .h(px(26.))
                                    .gap(px(3.))
                                    .child(
                                        toggle_button("search-case", "Aa", self.case_sensitive, cx)
                                            .on_click(cx.listener(|this, _, _w, cx| {
                                                this.case_sensitive = !this.case_sensitive;
                                                this.start_search(cx);
                                            }))
                                            .tooltip(simple_tooltip(
                                                tooltip_text::SEARCH_CASE_SENSITIVE,
                                            )),
                                    )
                                    .child(
                                        toggle_button("search-word", "ab|", self.whole_word, cx)
                                            .on_click(cx.listener(|this, _, _w, cx| {
                                                this.whole_word = !this.whole_word;
                                                this.start_search(cx);
                                            }))
                                            .tooltip(simple_tooltip(
                                                tooltip_text::SEARCH_WHOLE_WORD,
                                            )),
                                    )
                                    .child(
                                        toggle_button("search-regex", ".*", self.is_regex, cx)
                                            .on_click(cx.listener(|this, _, _w, cx| {
                                                this.is_regex = !this.is_regex;
                                                this.start_search(cx);
                                            }))
                                            .tooltip(simple_tooltip(tooltip_text::SEARCH_REGEX)),
                                    ),
                            )
                            .when(show_replace, |el| {
                                el.child(
                                    primary_button(
                                        "search-replace-all",
                                        "すべて置換",
                                        has_pattern,
                                        cx,
                                    )
                                    .on_click(cx.listener(|this, _, _w, cx| this.replace_all(cx))),
                                )
                            }),
                    ),
            )
            .child(
                h_flex().justify_end().child(
                    ghost_button(
                        "search-toggle-filters",
                        if self.show_filters {
                            "絞り込みを閉じる"
                        } else {
                            "絞り込み"
                        },
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.show_filters = !this.show_filters;
                        cx.notify();
                    })),
                ),
            )
            .when(self.show_filters, |el| {
                el.child(self.render_filter_row("含める", &self.include_input, &theme, cx))
                    .child(self.render_filter_row("除外する", &self.exclude_input, &theme, cx))
            })
            .children(self.notice.as_ref().map(|notice| {
                div()
                    .text_size(px(10.5))
                    .text_color(theme.accent_tertiary)
                    .child(notice.clone())
            }))
            .into_any_element()
    }

    fn render_filter_row(
        &self,
        label: &'static str,
        input: &Entity<TextInput>,
        theme: &Theme,
        cx: &App,
    ) -> AnyElement {
        v_flex()
            .gap(px(3.))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(label),
            )
            .child(input_box(input, theme, cx))
            .into_any_element()
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.status == SearchStatus::Unsupported {
            return empty_state(
                "ripgrep (rg) が見つかりません\nインストールするとワークスペース検索が使えます",
                cx,
            )
            .into_any_element();
        }
        if self.rows.is_empty() {
            let message = match &self.status {
                SearchStatus::Idle => "検索語を入力してください".to_string(),
                SearchStatus::Running => "検索中…".to_string(),
                SearchStatus::Finished { .. } => "一致するものがありません".to_string(),
                SearchStatus::Failed(message) => message.clone(),
                SearchStatus::Unsupported => String::new(),
            };
            return empty_state(message, cx).into_any_element();
        }

        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "search-results",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        // テーマは行ごとに引かず、可視範囲ぶんで 1 回だけ複製する。
                        let theme = theme(cx).clone();
                        range
                            .filter_map(|index| {
                                let row = *this.rows.get(index)?;
                                Some(match row {
                                    SearchRow::Header { group } => {
                                        this.render_group_header(index, group, &theme, cx)
                                    }
                                    SearchRow::Match {
                                        group,
                                        index: entry,
                                    } => this.render_match_row(index, group, entry, &theme, cx),
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
            return div().into_any_element();
        };
        let (dir, name) = split_path(&entry.relative);
        let count = entry.matches.len();
        let collapsed = entry.collapsed;

        list_row(("search-row", row), false, cx)
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
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(dir.to_string()),
            )
            .child(
                // 件数はネオンの小さな錠剤で出す。数字だけだと見出しに埋もれる。
                div()
                    .flex_none()
                    .px(px(5.))
                    .rounded(px(8.))
                    .bg(theme.accent_soft)
                    .text_size(px(10.))
                    .text_color(theme.accent)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _w, cx| this.toggle_group(group, cx)))
            .into_any_element()
    }

    fn render_match_row(
        &self,
        row: usize,
        group: usize,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.groups.get(group).and_then(|g| g.matches.get(index)) else {
            return div().into_any_element();
        };
        let (display, ranges) =
            prepare_match_line(&entry.line_text, &entry.matches, MAX_LINE_CHARS);
        let highlight = HighlightStyle {
            background_color: Some(theme.accent_soft),
            color: Some(theme.accent),
            ..Default::default()
        };
        let line_number = entry.line_number.to_string();

        list_row(("search-row", row), false, cx)
            .pl(px(26.))
            .child(
                div()
                    .w(px(34.))
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(theme.line_number)
                    .child(line_number),
            )
            .child(
                // 折り返すと行の高さが崩れて uniform_list の前提が壊れる。
                div().flex_1().truncate().child(
                    StyledText::new(display)
                        .with_highlights(ranges.into_iter().map(move |range| (range, highlight))),
                ),
            )
            .on_click(cx.listener(move |this, _, _w, cx| this.open_match(group, index, cx)))
            .into_any_element()
    }
}

/// 検索条件のトグル。有効なときだけネオンで光らせる。
fn toggle_button(
    id: impl Into<ElementId>,
    label: &'static str,
    active: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = theme(cx);
    h_flex()
        .id(id)
        .justify_center()
        .h(px(20.))
        .min_w(px(26.))
        .px(px(4.))
        .rounded(px(4.))
        .border_1()
        .text_size(px(10.))
        .cursor_pointer()
        .when(active, |el| {
            el.bg(theme.accent_soft)
                .border_color(theme.accent)
                .text_color(theme.accent)
        })
        .when(!active, |el| {
            el.border_color(theme.border)
                .text_color(theme.text_faint)
                .hover(|s| s.bg(theme.bg_overlay).text_color(theme.text_muted))
        })
        .child(label)
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 検索ビューを開いたら打ち始められるようにする。描画中に焦点を移すと
        // 再入するので、効果サイクルの終わりへ回す。
        if !self.focus_placed {
            self.focus_placed = true;
            let input = self.query_input.clone();
            cx.defer_in(window, move |_this, window, cx| {
                input.read(cx).focus(window);
            });
        }

        let theme = theme(cx).clone();
        let header = self.render_header(cx);
        let controls = self.render_controls(cx);
        let results = self.render_results(cx);
        let running = self.status == SearchStatus::Running;

        v_flex()
            .size_full()
            .overflow_hidden()
            // 上端の淡い光。暗い面が広いままだと計器盤らしさが出ない。
            .child(nebula_accent_line(if running {
                theme.accent
            } else {
                theme.accent_soft
            }))
            .child(header)
            .child(controls)
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .child(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 一致(path: &str, line: u32, text: &str, ranges: &[(usize, usize)]) -> SearchMatch {
        SearchMatch {
            path: PathBuf::from(path),
            line_number: line,
            line_text: text.to_string(),
            matches: ranges.iter().map(|(s, e)| TextRange::new(*s, *e)).collect(),
        }
    }

    fn 塊(path: &str, count: usize) -> FileGroup {
        FileGroup {
            path: PathBuf::from(path),
            relative: path.to_string(),
            matches: (0..count)
                .map(|i| 一致(path, i as u32 + 1, "x", &[(0, 1)]))
                .collect(),
            collapsed: false,
        }
    }

    #[test]
    fn 平坦化は見出しと一致行を交互に並べる() {
        let groups = vec![塊("a.rs", 2), 塊("b.rs", 1)];
        let rows = flatten_rows(&groups);
        assert_eq!(
            rows,
            vec![
                SearchRow::Header { group: 0 },
                SearchRow::Match { group: 0, index: 0 },
                SearchRow::Match { group: 0, index: 1 },
                SearchRow::Header { group: 1 },
                SearchRow::Match { group: 1, index: 0 },
            ]
        );
    }

    #[test]
    fn 折り畳んだ塊は見出しだけになる() {
        let mut groups = vec![塊("a.rs", 3), 塊("b.rs", 1)];
        groups[0].collapsed = true;
        let rows = flatten_rows(&groups);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], SearchRow::Header { group: 0 });
        assert_eq!(rows[1], SearchRow::Header { group: 1 });
    }

    #[test]
    fn 増分はファイルごとにまとまる() {
        let root = Path::new("/w");
        let mut groups = Vec::new();
        merge_matches(
            &mut groups,
            root,
            vec![
                一致("/w/src/a.rs", 1, "a", &[(0, 1)]),
                一致("/w/src/a.rs", 5, "a", &[(0, 1)]),
            ],
        );
        merge_matches(&mut groups, root, vec![一致("/w/b.rs", 2, "a", &[(0, 1)])]);
        // 既に出たファイルへ後から追記されても塊は増えない。
        merge_matches(
            &mut groups,
            root,
            vec![一致("/w/src/a.rs", 9, "a", &[(0, 1)])],
        );

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].relative, "src/a.rs");
        assert_eq!(groups[0].matches.len(), 3);
        assert_eq!(groups[1].matches.len(), 1);
    }

    #[test]
    fn 一致範囲は日本語行でも文字境界を割らない() {
        // 「検索」の位置は文字オフセットで 3..5。バイトでは 9..15。
        let line = "これは検索の対象です";
        let (display, ranges) = prepare_match_line(line, &[TextRange::new(3, 5)], MAX_LINE_CHARS);
        assert_eq!(display, line);
        assert_eq!(ranges, vec![9..15]);
        assert_eq!(&display[ranges[0].clone()], "検索");
    }

    #[test]
    fn 行頭のインデントは落とす() {
        let line = "\t    let x = 1;";
        // "let" は文字オフセット 5..8。
        let (display, ranges) = prepare_match_line(line, &[TextRange::new(5, 8)], MAX_LINE_CHARS);
        assert_eq!(display, "let x = 1;");
        assert_eq!(&display[ranges[0].clone()], "let");
    }

    #[test]
    fn 長い行は一致箇所が見える窓に切る() {
        let line = format!("{}TARGET{}", "a".repeat(400), "b".repeat(400));
        let (display, ranges) = prepare_match_line(&line, &[TextRange::new(400, 406)], 40);
        assert!(display.starts_with('…'), "手前を切ったら省略記号を付ける");
        assert!(display.ends_with('…'), "後ろを切ったら省略記号を付ける");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&display[ranges[0].clone()], "TARGET");
    }

    #[test]
    fn 窓の外に出た一致は捨てられる() {
        let line = "x".repeat(100);
        let (_, ranges) =
            prepare_match_line(&line, &[TextRange::new(0, 2), TextRange::new(90, 95)], 10);
        assert_eq!(ranges, vec![0..2]);
    }

    #[test]
    fn 重なった一致範囲は畳まれる() {
        let spans = normalize_ranges(
            &[
                TextRange::new(5, 9),
                TextRange::new(0, 3),
                TextRange::new(2, 6),
                TextRange::new(7, 7),
            ],
            20,
        );
        assert_eq!(spans, vec![0..9]);
    }

    #[test]
    fn 範囲外の一致は行の長さで切られる() {
        assert_eq!(normalize_ranges(&[TextRange::new(3, 99)], 10), vec![3..10]);
    }

    #[test]
    fn glob_はカンマ区切りで空要素を捨てる() {
        assert_eq!(
            parse_globs(" src/**/*.rs , , target "),
            vec!["src/**/*.rs".to_string(), "target".to_string()]
        );
        assert!(parse_globs("   ").is_empty());
    }

    #[test]
    fn 相対表示はルートを取り除く() {
        assert_eq!(
            relative_display(Path::new("/w"), Path::new("/w/src/a.rs")),
            "src/a.rs"
        );
        // ルート配下でないものはそのまま出す。
        assert_eq!(
            relative_display(Path::new("/w"), Path::new("/other/a.rs")),
            "/other/a.rs"
        );
    }

    #[test]
    fn パスは親とファイル名に分かれる() {
        assert_eq!(
            split_path("src/views/search.rs"),
            ("src/views", "search.rs")
        );
        assert_eq!(split_path("README.md"), ("", "README.md"));
    }

    #[test]
    fn 飛び先は_0_始まりの行と最初の一致位置() {
        let entry = 一致("/w/a.rs", 12, "  let x", &[(6, 7), (2, 5)]);
        let position = match_position(&entry);
        assert_eq!(position.row, 11);
        // 与えられた順の先頭を使う。rg は行内で昇順に出す。
        assert_eq!(position.column, 6);
    }

    #[test]
    fn 一致が無い行でも飛び先は行頭になる() {
        let entry = 一致("/w/a.rs", 1, "x", &[]);
        assert_eq!(match_position(&entry), Position::new(0, 0));
    }
}
