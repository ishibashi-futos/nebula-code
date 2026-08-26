//! コマンドパレット / クイックオープン。
//!
//! 画面中央上に浮かぶオーバーレイ。VS Code と同じく **入力欄ひとつで 2 つのモードを
//! 兼ねる**。先頭が `>` ならコマンド、そうでなければファイル名の絞り込みになる。
//! モードを別フィールドで持たず入力文字列から毎回導くのは、「`>` を消したのに
//! コマンドモードのまま」といった状態の食い違いを構造的に起こさないため。
//!
//! キー操作 (上下・確定・取り消し) はアクションではなく `on_key_down` で直接扱う。
//! アクションに割り当てるとエディタのキーバインドと衝突するうえ、パレットが開いて
//! いる間だけ有効にする仕組みを別に用意することになる。
//!
//! 絞り込みとスコアリング、強調位置の切り出しは描画から独立した純粋関数に置き、
//! 単体テストで固めてある。gpui に触れる部分はテストしない。

use crate::actions::keys;
use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::theme;
use crate::ui::{
    InputBackTab, InputDown, InputEnter, InputEscape, InputKey, InputNextItem, InputPrevItem,
    InputTab, InputUp, TextInput, TextInputEvent, format_keystroke, h_flex, icon, list_row,
    truncate_middle, v_flex,
};
use gpui::prelude::*;
use gpui::{
    App, BoxShadow, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, MouseButton,
    MouseDownEvent, Pixels, ScrollStrategy, Subscription, Task, UniformListScrollHandle, Window,
    div, point, px, relative, transparent_black, uniform_list,
};
use nebula_protocol::{FileCandidate, Request, Response, WorkspaceInfo};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 公開型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    Files,
}

pub enum PaletteEvent {
    OpenFile(PathBuf),
    RunCommand(String),
    Dismissed,
}

// ---------------------------------------------------------------------------
// 寸法と定数
// ---------------------------------------------------------------------------

/// 入力が止まってから検索するまでの待ち。打鍵のたびにワークスペース全体の
/// 走査を起こすと、大きなリポジトリで入力が引っかかる。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(100);
/// 取得するファイル候補の上限。
const FILE_LIMIT: usize = 50;
const PANEL_WIDTH: Pixels = px(600.);
const ROW_HEIGHT: Pixels = px(30.);
/// 一覧に見せる最大行数。これを超えるぶんはスクロールで辿る。
const VISIBLE_ROWS: usize = 10;
/// 入力欄の行の高さ。カーソルの縦幅もこれに合わせる。
const INPUT_LINE_HEIGHT: Pixels = px(22.);
const INPUT_FONT_SIZE: Pixels = px(15.);

/// コマンドパレットに並べる操作。
///
/// `id` は app.rs / EditorArea が解釈する文字列。ここを間違えると無反応になるので
/// 一覧の内容は単体テストで固定してある。
///
/// 表示名を英語と日本語の2フィールドに分けているのは、検索（[`command_label`] で
/// 結合した文字列に対する部分列一致）と描画（英語側だけ淡色にする、
/// [`render_command_row`] 参照）の両方で「区切り位置」を文字列パースし直さずに
/// 済ませるため。
struct CommandDef {
    id: &'static str,
    /// VS Code のコマンド名慣習に寄せた英語名。ASCII のみ（単体テストで検査する）。
    english: &'static str,
    /// 動作が分かる日本語の動詞句。
    japanese: &'static str,
    /// 既定のキーバインド。表示のためだけに持つ。
    ///
    /// 文字列を直に書かず `actions::keys` から取る。ここに書き写していた頃は、
    /// actions.rs 側で打鍵を変えてもパレットの表示だけ古いまま残った。
    keystroke: Option<&'static str>,
}

/// 検索・表示に使う結合ラベル。`"{english}: {japanese}"` の形を一箇所に固定する。
fn command_label(command: &CommandDef) -> String {
    format!("{}: {}", command.english, command.japanese)
}

const COMMANDS: &[CommandDef] = &[
    CommandDef {
        id: "view.explorer",
        english: "Explorer",
        japanese: "エクスプローラーを表示",
        keystroke: Some(keys::SHOW_EXPLORER),
    },
    CommandDef {
        id: "view.search",
        english: "Search",
        japanese: "検索を表示",
        keystroke: Some(keys::SHOW_SEARCH),
    },
    CommandDef {
        id: "view.git",
        english: "Source Control",
        japanese: "ソース管理を開く",
        keystroke: Some(keys::SHOW_GIT),
    },
    CommandDef {
        id: "view.codex",
        english: "Codex",
        japanese: "Codexを開く",
        keystroke: Some(keys::SHOW_CODEX),
    },
    CommandDef {
        id: "view.terminal",
        english: "Terminal",
        japanese: "ターミナルを開く",
        keystroke: Some(keys::TOGGLE_TERMINAL),
    },
    CommandDef {
        id: "view.problems",
        english: "Problems",
        japanese: "問題を表示",
        keystroke: None,
    },
    CommandDef {
        id: "view.toggleSidebar",
        english: "Toggle Sidebar",
        japanese: "サイドバーの表示切り替え",
        keystroke: Some(keys::TOGGLE_SIDEBAR),
    },
    CommandDef {
        id: "editor.save",
        english: "Save",
        japanese: "保存する",
        keystroke: Some(keys::SAVE),
    },
    CommandDef {
        id: "editor.close",
        english: "Close Editor",
        japanese: "タブを閉じる",
        keystroke: Some(keys::CLOSE_TAB),
    },
    CommandDef {
        id: "editor.splitRight",
        english: "Split Right",
        japanese: "右に分割",
        keystroke: Some(keys::SPLIT_RIGHT),
    },
    CommandDef {
        id: "editor.togglePreview",
        english: "Toggle Markdown Preview",
        japanese: "Markdownプレビューの表示切り替え",
        keystroke: Some(keys::TOGGLE_PREVIEW),
    },
    CommandDef {
        id: "editor.nextTab",
        english: "Next Editor",
        japanese: "次のタブへ移動",
        keystroke: Some(keys::NEXT_TAB),
    },
    CommandDef {
        id: "editor.previousTab",
        english: "Previous Editor",
        japanese: "前のタブへ移動",
        keystroke: Some(keys::PREVIOUS_TAB),
    },
];

// ---------------------------------------------------------------------------
// 純粋関数: モード判定・あいまい一致・強調区間
// ---------------------------------------------------------------------------

/// 入力文字列からモードと絞り込み語を取り出す。
///
/// モードを状態として持たずここで毎回導くことで、`>` の付け外しがそのまま
/// モード切り替えになる。
fn parse_input(input: &str) -> (PaletteMode, &str) {
    match input.strip_prefix('>') {
        Some(rest) => (PaletteMode::Commands, rest.trim()),
        None => (PaletteMode::Files, input.trim()),
    }
}

/// あいまい一致の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FuzzyMatch {
    score: i32,
    /// 一致した **文字位置** (バイト位置ではない)。
    /// プロトコルの `FileCandidate::match_positions` と同じ単位に揃えてある。
    positions: Vec<usize>,
}

/// 語の区切りの直後かどうか。
///
/// 区切り直後の一致を強く優遇するのは、`vs` で `view.search` を引き当てたい
/// という「頭文字で辿る」使い方が最も多いため。
fn is_boundary(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = chars[index - 1];
    if matches!(previous, ' ' | '/' | '\\' | '_' | '-' | '.' | ':' | '　') {
        return true;
    }
    previous.is_lowercase() && chars[index].is_uppercase()
}

/// `needle` が `haystack` の部分列として現れるか調べ、見つかればスコアを付ける。
///
/// 大文字小文字は無視する。左から貪欲に拾うので最適解とは限らないが、
/// 結果が入力に対して決定的で、候補数が数千でも一定時間で終わる。
fn fuzzy_match(haystack: &str, needle: &str) -> Option<FuzzyMatch> {
    let hay: Vec<char> = haystack.chars().collect();
    let hay_lower: Vec<char> = hay
        .iter()
        .flat_map(|c| c.to_lowercase())
        .collect::<Vec<_>>();
    // to_lowercase は 1 文字が複数文字になりうる。位置がずれると強調が破綻するので
    // その場合は元の文字をそのまま使う。
    let hay_lower = if hay_lower.len() == hay.len() {
        hay_lower
    } else {
        hay.clone()
    };

    let pattern: Vec<char> = needle
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    if pattern.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }

    let mut positions = Vec::with_capacity(pattern.len());
    let mut score = 0i32;
    let mut cursor = 0usize;
    let mut previous: Option<usize> = None;

    for target in pattern {
        let found = (cursor..hay.len()).find(|&i| hay_lower[i] == target)?;
        score += 10;
        if is_boundary(&hay, found) {
            score += 12;
        }
        if let Some(previous) = previous {
            if found == previous + 1 {
                score += 8;
            } else {
                // 飛ばした文字ぶんだけ減点する。離れた一致は「たまたま」に近い。
                score -= ((found - previous - 1) as i32).min(8);
            }
        }
        positions.push(found);
        previous = Some(found);
        cursor = found + 1;
    }

    // 同じ当たり方なら短い候補を優先する。
    score -= (hay.len().saturating_sub(positions.len()) as i32) / 4;
    Some(FuzzyMatch { score, positions })
}

/// 絞り込んだコマンド 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandHit {
    /// [`COMMANDS`] 内の添字。
    index: usize,
    score: i32,
    /// 表示名のうち強調する文字位置。
    positions: Vec<usize>,
}

/// コマンド表を絞り込む。
///
/// 表示名で当たればそれを優先し、当たらなければ識別子で拾う (`save` で「保存」を
/// 出すため)。識別子で拾った場合は表示名に強調すべき位置がないので空にする。
fn filter_commands(commands: &[CommandDef], query: &str) -> Vec<CommandHit> {
    let mut hits: Vec<CommandHit> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            if let Some(matched) = fuzzy_match(&command_label(command), query) {
                return Some(CommandHit {
                    index,
                    // 表示名での一致は識別子での一致より確実に上に来るようにする。
                    score: matched.score + 20,
                    positions: matched.positions,
                });
            }
            fuzzy_match(command.id, query).map(|matched| CommandHit {
                index,
                score: matched.score,
                positions: Vec::new(),
            })
        })
        .collect();
    // 安定ソートなので、同点なら表の並び順が保たれる。
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.score));
    hits
}

/// 結合ラベル（`"{english}: {japanese}"`）に対する文字位置の強調を、英語側・
/// 日本語側それぞれの文字列内での位置に分割する。
///
/// `english_char_len` は英語部分の文字数（ASCII のみなのでバイト数と一致するが、
/// 呼び出し側は `chars().count()` を渡すこと）。区切り文字列 `": "` の 2 文字に
/// かかった位置（コロン自体・直後の空白）はどちらの側にも属さないため捨てる。
fn split_highlight_positions(
    english_char_len: usize,
    positions: &[usize],
) -> (Vec<usize>, Vec<usize>) {
    const SEPARATOR_LEN: usize = 2; // ": "
    let japanese_start = english_char_len + SEPARATOR_LEN;
    let english_positions = positions
        .iter()
        .filter(|&&p| p < english_char_len)
        .copied()
        .collect();
    let japanese_positions = positions
        .iter()
        .filter(|&&p| p >= japanese_start)
        .map(|&p| p - japanese_start)
        .collect();
    (english_positions, japanese_positions)
}

/// 強調表示のための区間列。`(部分文字列, 一致しているか)`。
fn highlight_segments(text: &str, positions: &[usize]) -> Vec<(String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut marked = vec![false; chars.len()];
    for &position in positions {
        if position < marked.len() {
            marked[position] = true;
        }
    }

    let mut segments: Vec<(String, bool)> = Vec::new();
    for (index, ch) in chars.into_iter().enumerate() {
        let hit = marked[index];
        match segments.last_mut() {
            Some((buffer, last_hit)) if *last_hit == hit => buffer.push(ch),
            _ => segments.push((ch.to_string(), hit)),
        }
    }
    segments
}

/// 相対パスを「ディレクトリ部」「ファイル名」「ファイル名の開始文字位置」に分ける。
///
/// 強調位置は相対パス全体に対する文字位置で来るので、分割後も対応付けられるよう
/// 開始位置を返す。
fn split_relative(relative: &str) -> (String, String, usize) {
    match relative.rfind('/') {
        Some(byte_index) => {
            let directory = &relative[..byte_index];
            let name = &relative[byte_index + 1..];
            (
                directory.to_string(),
                name.to_string(),
                directory.chars().count() + 1,
            )
        }
        None => (String::new(), relative.to_string(), 0),
    }
}

/// 全体に対する強調位置を、部分文字列に対する位置へ写す。
fn shift_positions(positions: &[u32], start: usize, len: usize) -> Vec<usize> {
    positions
        .iter()
        .map(|p| *p as usize)
        .filter(|p| *p >= start && *p < start + len)
        .map(|p| p - start)
        .collect()
}

// ---------------------------------------------------------------------------
// ビュー
// ---------------------------------------------------------------------------

pub struct CommandPalette {
    client: Option<BackendClient>,
    /// 検索対象。app.rs から配られないので `ListWorkspaces` で自前に引く。
    workspace: Option<WorkspaceInfo>,
    open: bool,

    /// 絞り込みの入力欄。モードもここの中身から導く。
    input: Entity<TextInput>,

    files: Vec<FileCandidate>,
    /// 一覧内の選択位置。
    selected: usize,
    searching: bool,
    list_scroll: UniformListScrollHandle,

    focus_handle: FocusHandle,
    /// 開く前にフォーカスしていた場所。閉じたら戻す。
    previous_focus: Option<FocusHandle>,

    /// 入力欄の購読。保持しないと即座に解除される。
    _subscription: Subscription,

    /// 遅れて届いた古い応答を捨てるための世代番号。
    search_generation: u64,
    /// 間引き中の検索。捨てると (= 置き換えると) 取り消される。
    _search_task: Option<Task<()>>,
    _workspace_task: Option<Task<()>>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CommandPalette {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Enter は候補の確定、Escape は取り消し、上下は候補送り。いずれも
        // 入力欄ではなくパレットの仕事なので、入力欄には処理させない。
        let input = cx.new(|cx| {
            TextInput::single_line("コマンドまたはファイル名", cx).reserving(&[
                InputKey::Enter,
                InputKey::Escape,
                InputKey::Up,
                InputKey::Down,
            ])
        });
        let subscription = cx.subscribe(&input, |this, _input, event, cx| {
            if matches!(event, TextInputEvent::Changed) {
                this.on_query_changed(cx);
            }
        });
        Self {
            client: None,
            workspace: None,
            open: false,
            input,
            _subscription: subscription,
            files: Vec::new(),
            selected: 0,
            searching: false,
            list_scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            previous_focus: None,
            search_generation: 0,
            _search_task: None,
            _workspace_task: None,
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        self.refresh_workspace(cx);
        cx.notify();
    }

    /// ワークスペースを外から与える。
    ///
    /// app.rs は現状これを呼ばないので、[`Self::refresh_workspace`] が既定の経路。
    /// 他のビューと同じ形を残しておき、配線が入ったときにそのまま使えるようにする。
    #[allow(dead_code)]
    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        cx.notify();
    }

    pub fn open(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // 入力欄の初期値がそのままモードになる。書き換えは
        // [`TextInputEvent::Changed`] を通じて [`Self::on_query_changed`] を呼ぶ。
        let initial = match mode {
            PaletteMode::Commands => ">",
            PaletteMode::Files => "",
        };
        self.input
            .update(cx, |input, cx| input.set_text(initial, cx));
        self.files.clear();
        self.selected = 0;

        if !self.open {
            self.previous_focus = window.focused(cx);
        }
        self.open = true;
        // IME を通すため、焦点はパレットではなく入力欄そのものへ移す。
        self.input.read(cx).focus(window);

        // ワークスペースはパレットを開いた時点の最新を見る。GUI 起動直後は
        // まだ 1 つも開かれていないことがあるため、接続時の 1 回では足りない。
        self.refresh_workspace(cx);
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self._search_task = None;
        self.searching = false;
        // フォーカスを戻さないと、閉じたあとエディタのキー操作が効かなくなる。
        if let Some(previous) = self.previous_focus.take() {
            window.focus(&previous);
        }
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.close(window, cx);
        cx.emit(PaletteEvent::Dismissed);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode(cx) {
            PaletteMode::Commands => {
                let Some(hit) = self.command_hits(cx).get(self.selected).cloned() else {
                    return;
                };
                let id = COMMANDS[hit.index].id.to_string();
                self.close(window, cx);
                cx.emit(PaletteEvent::RunCommand(id));
            }
            PaletteMode::Files => {
                let Some(candidate) = self.files.get(self.selected) else {
                    return;
                };
                let path = candidate.path.clone();
                self.close(window, cx);
                cx.emit(PaletteEvent::OpenFile(path));
            }
        }
    }

    // -- 状態の導出 --

    /// 入力欄の中身。モードと絞り込み語はここから導く。
    fn query<'a>(&self, cx: &'a App) -> &'a str {
        self.input.read(cx).text()
    }

    fn mode(&self, cx: &App) -> PaletteMode {
        parse_input(self.query(cx)).0
    }

    fn command_hits(&self, cx: &App) -> Vec<CommandHit> {
        filter_commands(COMMANDS, parse_input(self.query(cx)).1)
    }

    fn result_count(&self, cx: &App) -> usize {
        match self.mode(cx) {
            PaletteMode::Commands => self.command_hits(cx).len(),
            PaletteMode::Files => self.files.len(),
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.result_count(cx);
        if count == 0 {
            self.selected = 0;
            return;
        }
        // 端で折り返す。候補が少ないときに下端で止まるより辿りやすい。
        let next = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.selected = next;
        self.list_scroll.scroll_to_item(next, ScrollStrategy::Top);
        cx.notify();
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    // -- バックエンドとのやり取り --

    fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self._workspace_task = Some(cx.spawn(async move |this, cx| {
            let result = client.request(Request::ListWorkspaces).await;
            this.update(cx, |this, cx| {
                if let Ok(Response::Workspaces(workspaces)) = result {
                    let first = workspaces.into_iter().next();
                    let changed =
                        first.as_ref().map(|w| w.id) != this.workspace.as_ref().map(|w| w.id);
                    this.workspace = first;
                    if changed && this.open && this.mode(cx) == PaletteMode::Files {
                        this.start_file_search(cx);
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// 入力が変わったときの共通処理。
    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.reset_selection();
        if self.mode(cx) == PaletteMode::Files {
            self.start_file_search(cx);
        } else {
            self._search_task = None;
            self.searching = false;
        }
        cx.notify();
    }

    /// ファイル候補を取り直す。前の要求は Task を置き換えることで取り消される。
    fn start_file_search(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) =
            (self.client.clone(), self.workspace.as_ref().map(|w| w.id))
        else {
            self.files.clear();
            self.searching = false;
            return;
        };
        let query = parse_input(self.query(cx)).1.to_string();
        self.search_generation += 1;
        let generation = self.search_generation;
        self.searching = true;

        self._search_task = Some(cx.spawn(async move |this, cx| {
            // 打鍵が続いている間は投げない。ここで待つ間に Task ごと捨てられる。
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let result = client
                .request(Request::FindFiles {
                    workspace,
                    query,
                    limit: FILE_LIMIT,
                })
                .await;
            this.update(cx, |this, cx| {
                // 待っている間に入力が進んでいたら古い結果なので捨てる。
                if this.search_generation != generation {
                    return;
                }
                this.searching = false;
                if let Ok(Response::FileCandidates(candidates)) = result {
                    this.files = candidates;
                    this.reset_selection();
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // -- 入力欄から流れてきたキー --

    fn on_confirm(&mut self, _: &InputEnter, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(window, cx);
    }

    fn on_dismiss(&mut self, _: &InputEscape, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(window, cx);
    }

    fn on_prev_item(&mut self, _: &InputUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_next_item(&mut self, _: &InputDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    /// Emacs 風の上下。ホームポジションから外れずに辿れる。
    fn on_ctrl_p(&mut self, _: &InputPrevItem, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_ctrl_n(&mut self, _: &InputNextItem, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_tab(&mut self, _: &InputTab, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_back_tab(&mut self, _: &InputBackTab, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    // -- 描画 --

    /// 入力欄の行。左に現在のモードを示す印を置く。
    fn render_input(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let prefix = match self.mode(cx) {
            PaletteMode::Files => icon(Icon::Search, px(15.), theme.accent).into_any_element(),
            PaletteMode::Commands => div()
                .w(px(15.))
                .text_size(px(16.))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.accent)
                .child(">")
                .into_any_element(),
        };

        h_flex()
            .w_full()
            .h(px(46.))
            .flex_none()
            .px(px(14.))
            .gap(px(10.))
            .child(prefix)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(INPUT_FONT_SIZE)
                    .line_height(INPUT_LINE_HEIGHT)
                    .text_color(theme.text)
                    .child(self.input.clone()),
            )
            .into_any_element()
    }

    /// 一致した文字を [`Theme::accent`] で光らせた 1 行。
    fn render_highlighted(
        &self,
        text: &str,
        positions: &[usize],
        base: Hsla,
        size: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let accent = theme(cx).accent;
        let segments = highlight_segments(text, positions);
        h_flex()
            .text_size(size)
            .children(segments.into_iter().map(|(chunk, hit)| {
                div()
                    .text_color(if hit { accent } else { base })
                    .when(hit, |el| el.font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(chunk)
            }))
            .into_any_element()
    }

    fn render_command_row(
        &self,
        position: usize,
        hit: &CommandHit,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let selected = position == self.selected;
        let command = &COMMANDS[hit.index];
        // 種類でアイコンを変え、見た目だけで「画面の操作」と「編集の操作」を分ける。
        let glyph = if command.id.starts_with("view.") {
            Icon::Panel
        } else {
            Icon::Edit
        };
        // 英語名: 日本語名 を別の見た目で描く。英語側は常に淡色（theme.text_faint）
        // にして「補助情報」だと分かるようにし、日本語側だけ選択状態に応じた
        // 通常の文字色にする。ハイライト位置は結合ラベル基準の文字位置なので、
        // split_highlight_positions で英語側/日本語側それぞれの相対位置に
        // 付け替えてから render_highlighted に渡す（区切り文字自体にかかった
        // 位置は split_highlight_positions が捨てる）。
        let english_char_len = command.english.chars().count();
        let (english_positions, japanese_positions) =
            split_highlight_positions(english_char_len, &hit.positions);
        let japanese_base = if selected {
            theme.text
        } else {
            theme.text_muted
        };
        // gap は張らない。h_flex に gap を張るとコロンの前後**両方**に隙間が入り
        // "Terminal : ターミナルを開く" と間延びして見える。コロンは英語名に密着させ、
        // 日本語名との間だけを右マージンで空ける。
        let label = h_flex()
            .child(self.render_highlighted(
                command.english,
                &english_positions,
                theme.text_faint,
                px(13.),
                cx,
            ))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.text_faint)
                    .mr(px(4.))
                    .child(":"),
            )
            .child(self.render_highlighted(
                command.japanese,
                &japanese_positions,
                japanese_base,
                px(13.),
                cx,
            ))
            .into_any_element();

        list_row(("palette-command", position), selected, cx)
            .h(ROW_HEIGHT)
            .overflow_hidden()
            .gap(px(9.))
            .border_l_2()
            .border_color(if selected {
                theme.accent
            } else {
                transparent_black()
            })
            .child(icon(
                glyph,
                px(13.),
                if selected {
                    theme.accent
                } else {
                    theme.text_faint
                },
            ))
            .child(label)
            .child(div().flex_1())
            .children(command.keystroke.map(|keystroke| {
                h_flex()
                    .px(px(6.))
                    .h(px(18.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(format_keystroke(keystroke))
            }))
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.selected = position;
                this.confirm(window, cx);
            }))
            .into_any_element()
    }

    fn render_file_row(
        &self,
        position: usize,
        candidate: &FileCandidate,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let selected = position == self.selected;
        let (directory, name, name_start) = split_relative(&candidate.relative);

        let name_positions =
            shift_positions(&candidate.match_positions, name_start, name.chars().count());
        let name_element = self.render_highlighted(
            &name,
            &name_positions,
            if selected {
                theme.text
            } else {
                theme.text_muted
            },
            px(13.),
            cx,
        );

        // ディレクトリは長くなりがちなので中略する。中略すると強調位置が
        // ずれるため、中略が起きた場合は強調しない。
        let shortened = truncate_middle(&directory, 56);
        let directory_positions = if shortened == directory {
            shift_positions(&candidate.match_positions, 0, directory.chars().count())
        } else {
            Vec::new()
        };
        let directory_element = self.render_highlighted(
            &shortened,
            &directory_positions,
            theme.text_faint,
            px(11.),
            cx,
        );

        let path = candidate.path.clone();
        list_row(("palette-file", position), selected, cx)
            .h(ROW_HEIGHT)
            .overflow_hidden()
            .gap(px(9.))
            .border_l_2()
            .border_color(if selected {
                theme.accent
            } else {
                transparent_black()
            })
            .child(icon(
                Icon::File,
                px(13.),
                if selected {
                    theme.accent
                } else {
                    theme.text_faint
                },
            ))
            .child(name_element)
            .child(div().flex_1())
            .child(directory_element)
            .on_click(cx.listener(move |this, _event, window, cx| {
                let path = path.clone();
                this.close(window, cx);
                cx.emit(PaletteEvent::OpenFile(path));
            }))
            .into_any_element()
    }

    /// 候補一覧。数千件になりうるので必ず仮想化する。
    fn render_results(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let count = self.result_count(cx);

        if count == 0 {
            let message: String = match (self.mode(cx), self.workspace.is_some(), self.searching) {
                // 案内する打鍵は `actions::keys` から組み立てる。ここに `⌘O` と
                // 直書きすると、Windows では存在しない打鍵を案内してしまう。
                (PaletteMode::Files, false, _) => format!(
                    "フォルダが開かれていません\n{} でフォルダを開いてください",
                    format_keystroke(keys::OPEN_FOLDER)
                ),
                (PaletteMode::Files, true, true) => "検索中…".to_string(),
                (PaletteMode::Files, true, false) => "一致するファイルがありません".to_string(),
                (PaletteMode::Commands, _, _) => "一致するコマンドがありません".to_string(),
            };
            return div()
                .w_full()
                .h(px(72.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .text_color(theme.text_faint)
                .text_center()
                .child(message)
                .into_any_element();
        }

        let height = ROW_HEIGHT * (count.min(VISIBLE_ROWS) as f32);
        uniform_list(
            "palette-results",
            count,
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let hits = match this.mode(cx) {
                    PaletteMode::Commands => Some(this.command_hits(cx)),
                    PaletteMode::Files => None,
                };
                range
                    .map(|position| match &hits {
                        Some(hits) => match hits.get(position) {
                            Some(hit) => this.render_command_row(position, hit, cx),
                            None => div().into_any_element(),
                        },
                        // 範囲外はありえないが、要求と状態がずれた瞬間でも落ちないよう空で埋める。
                        None => match this.files.get(position) {
                            Some(candidate) => {
                                let candidate = candidate.clone();
                                this.render_file_row(position, &candidate, cx)
                            }
                            None => div().into_any_element(),
                        },
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .h(height)
        .track_scroll(self.list_scroll.clone())
        .into_any_element()
    }

    /// 下端の操作ヒント。
    fn render_footer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let hint = |keys: String, label: &'static str| {
            h_flex()
                .gap(px(4.))
                .child(
                    div()
                        .text_color(theme.accent)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(keys),
                )
                .child(div().text_color(theme.text_faint).child(label))
        };
        let mode_label = match self.mode(cx) {
            PaletteMode::Commands => "コマンド",
            PaletteMode::Files => "ファイル",
        };

        h_flex()
            .w_full()
            .h(px(26.))
            .flex_none()
            .px(px(14.))
            .gap(px(14.))
            .text_size(px(10.5))
            .border_t_1()
            .border_color(theme.border)
            .child(hint(
                format!("{}{}", format_keystroke("up"), format_keystroke("down")),
                "選択",
            ))
            .child(hint(format_keystroke("enter"), "決定"))
            .child(hint(format_keystroke("escape"), "閉じる"))
            .child(div().flex_1())
            .child(
                div()
                    .text_color(theme.text_faint)
                    .child(format!("{mode_label} · > でコマンド")),
            )
            .into_any_element()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = theme(cx).clone();
        let input = self.render_input(cx);
        let results = self.render_results(cx);
        let footer = self.render_footer(cx);

        let panel = v_flex()
            .w(PANEL_WIDTH)
            .max_w_full()
            .flex_none()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_glow)
            // 外側にシアンの淡い光を落として「宇宙船のコンソール」らしさを出す。
            // 下方向の濃い影は、暗い背景から浮いていることを伝えるため。
            .shadow(vec![
                BoxShadow {
                    color: theme.accent.alpha(0.22),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(30.),
                    spread_radius: px(0.),
                },
                BoxShadow {
                    color: theme.bg_void.alpha(0.8),
                    offset: point(px(0.), px(20.)),
                    blur_radius: px(44.),
                    spread_radius: px(0.),
                },
            ])
            // 上端のネオン 1 本。計器の電源が入っている感じを出す。
            .child(div().h(px(2.)).w_full().flex_none().bg(theme.accent))
            .child(input)
            .child(div().h(px(1.)).w_full().flex_none().bg(theme.border))
            .child(results)
            .child(footer)
            // パネル内のクリックで閉じないよう、背景まで通さない。
            .occlude();

        div()
            .key_context("Palette")
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .bg(theme.bg_void.alpha(0.72))
            // 入力欄が受け取らずに流したキー。予約したものは
            // [`TextInput::reserving`]、Tab や ⌃P は入力欄が最初から扱わない。
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_prev_item))
            .on_action(cx.listener(Self::on_next_item))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_back_tab))
            .on_action(cx.listener(Self::on_ctrl_p))
            .on_action(cx.listener(Self::on_ctrl_n))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                    this.dismiss(window, cx);
                }),
            )
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    // 画面の上から 15% の位置に出す。中央よりやや上のほうが
                    // 一覧が伸びても視線の移動が少ない。
                    .child(div().h(relative(0.15)).flex_none())
                    .child(panel),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 先頭の記号でモードが決まる() {
        assert_eq!(parse_input(""), (PaletteMode::Files, ""));
        assert_eq!(parse_input("main.rs"), (PaletteMode::Files, "main.rs"));
        assert_eq!(parse_input(">"), (PaletteMode::Commands, ""));
        assert_eq!(parse_input("> 保存"), (PaletteMode::Commands, "保存"));
    }

    #[test]
    fn 部分列でなければ一致しない() {
        assert!(fuzzy_match("view.search", "vs").is_some());
        assert!(fuzzy_match("view.search", "sv").is_none(), "順序が違う");
        assert!(fuzzy_match("view.search", "vz").is_none());
    }

    #[test]
    fn 空の入力は全てに一致する() {
        let matched = fuzzy_match("なんでも", "").expect("空入力は常に一致する");
        assert_eq!(matched.score, 0);
        assert!(matched.positions.is_empty());
    }

    #[test]
    fn 大文字小文字を無視する() {
        let matched = fuzzy_match("SplitRight", "sr").expect("一致するはず");
        assert_eq!(matched.positions, vec![0, 5]);
    }

    #[test]
    fn 連続した一致のほうが高得点() {
        let together = fuzzy_match("abcd", "ab").expect("一致するはず").score;
        let apart = fuzzy_match("axbd", "ab").expect("一致するはず").score;
        assert!(together > apart, "{together} > {apart}");
    }

    #[test]
    fn 語頭の一致のほうが高得点() {
        let boundary = fuzzy_match("foo bar", "b").expect("一致するはず").score;
        let middle = fuzzy_match("foobar", "b").expect("一致するはず").score;
        assert!(boundary > middle, "{boundary} > {middle}");
    }

    #[test]
    fn 一致位置は文字単位で返る() {
        // 「あ」「い」は 3 バイトずつ。バイト位置で数えていれば 6 になる。
        let matched = fuzzy_match("あいr", "r").expect("一致するはず");
        assert_eq!(matched.positions, vec![2]);
    }

    #[test]
    fn コマンド表に必要な識別子が揃っている() {
        let expected = [
            "view.explorer",
            "view.search",
            "view.git",
            "view.codex",
            "view.terminal",
            "view.problems",
            "view.toggleSidebar",
            "editor.save",
            "editor.close",
            "editor.splitRight",
            "editor.togglePreview",
            "editor.nextTab",
            "editor.previousTab",
        ];
        let ids: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn 空の入力では全コマンドが元の順で並ぶ() {
        let hits = filter_commands(COMMANDS, "");
        assert_eq!(hits.len(), COMMANDS.len());
        let indices: Vec<usize> = hits.iter().map(|h| h.index).collect();
        assert_eq!(indices, (0..COMMANDS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn 表示名で絞り込める() {
        let hits = filter_commands(COMMANDS, "エクスプ");
        assert!(!hits.is_empty());
        assert_eq!(COMMANDS[hits[0].index].id, "view.explorer");
        assert!(!hits[0].positions.is_empty(), "強調位置が付く");
    }

    #[test]
    fn 英語名を含む表示名で絞り込める() {
        // 英語名を "Split Right: 右に分割" のように表示名へ含めた結果、
        // 識別子へのフォールバックを待たずに表示名だけで一致するようになった。
        // これはこの issue が意図した改善そのものなので、強調位置は
        // 空ではなく付く方が正しい（フォールバック時のみ空になる）。
        let hits = filter_commands(COMMANDS, "splitright");
        assert_eq!(COMMANDS[hits[0].index].id, "editor.splitRight");
        assert!(!hits[0].positions.is_empty(), "表示名側に強調位置が付く");
    }

    #[test]
    fn 表示名に無い記号を使えば識別子だけで絞り込める() {
        // 結合ラベルは "英語名: 日本語名" で "." を含まないので、"." を含む
        // クエリは表示名側では絶対に一致せず、識別子フォールバックだけが働く。
        let hits = filter_commands(COMMANDS, "editor.split");
        assert_eq!(COMMANDS[hits[0].index].id, "editor.splitRight");
        assert!(
            hits[0].positions.is_empty(),
            "識別子で拾った場合は表示名に強調位置がない"
        );
    }

    #[test]
    fn 一致しない入力では候補が空になる() {
        assert!(filter_commands(COMMANDS, "zzzqqq").is_empty());
    }

    #[test]
    fn 英語名だけの入力で該当コマンドが1位に来る() {
        let hits = filter_commands(COMMANDS, "term");
        assert_eq!(COMMANDS[hits[0].index].id, "view.terminal");

        let hits = filter_commands(COMMANDS, "toggle sidebar");
        assert_eq!(COMMANDS[hits[0].index].id, "view.toggleSidebar");
    }

    #[test]
    fn 日本語名だけの入力で該当コマンドが1位に来る() {
        let hits = filter_commands(COMMANDS, "ターミナル");
        assert_eq!(COMMANDS[hits[0].index].id, "view.terminal");

        let hits = filter_commands(COMMANDS, "サイドバー");
        assert_eq!(COMMANDS[hits[0].index].id, "view.toggleSidebar");
    }

    #[test]
    fn 全コマンドの表示名は英語名コロン空白日本語名の形式を守る() {
        for command in COMMANDS {
            assert!(
                command.english.is_ascii(),
                "{} の英語名は ASCII のみであるべき",
                command.id
            );
            let label = command_label(command);
            assert_eq!(
                label.matches(": ").count(),
                1,
                "{} の表示名は \": \" をちょうど1つ含むべき: {label}",
                command.id
            );
        }
    }

    #[test]
    fn 結合ラベルの強調位置を英語側と日本語側へ分けられる() {
        // "Terminal: ターミナルを開く" の "Terminal" (0..8) と
        // 区切り文字列 ": " (8..10) と日本語部分 (10..) の境界をまたぐケース。
        let english_char_len = "Terminal".chars().count();
        let (english, japanese) = split_highlight_positions(english_char_len, &[0, 3, 10, 11]);
        assert_eq!(
            english,
            vec![0, 3],
            "英語側は英語部分内の位置がそのまま残る"
        );
        assert_eq!(
            japanese,
            vec![0, 1],
            "日本語側は区切り文字列ぶん (english_char_len + 2) だけ引いた位置になる"
        );
    }

    #[test]
    fn 区切り文字にかかった強調位置は捨てられる() {
        // english_char_len が 4 のとき、位置 4 (コロン) と 5 (空白) はどちらの
        // 側にも属さないので、双方の結果から消える。
        let (english, japanese) = split_highlight_positions(4, &[3, 4, 5, 6]);
        assert_eq!(english, vec![3]);
        assert_eq!(japanese, vec![0]);
    }

    #[test]
    fn 強調区間は連続する同種をまとめる() {
        let segments = highlight_segments("abcd", &[0, 1, 3]);
        assert_eq!(
            segments,
            vec![
                ("ab".to_string(), true),
                ("c".to_string(), false),
                ("d".to_string(), true),
            ]
        );
    }

    #[test]
    fn 強調区間は文字位置で切る() {
        // 「日」が 3 バイトでも、位置 1 は「本」を指す。
        let segments = highlight_segments("日本語", &[1]);
        assert_eq!(
            segments,
            vec![
                ("日".to_string(), false),
                ("本".to_string(), true),
                ("語".to_string(), false),
            ]
        );
    }

    #[test]
    fn 範囲外の強調位置は無視する() {
        assert_eq!(
            highlight_segments("ab", &[9]),
            vec![("ab".to_string(), false)]
        );
    }

    #[test]
    fn 相対パスをディレクトリとファイル名に分ける() {
        assert_eq!(
            split_relative("src/views/palette.rs"),
            ("src/views".to_string(), "palette.rs".to_string(), 10)
        );
        assert_eq!(
            split_relative("README.md"),
            (String::new(), "README.md".to_string(), 0)
        );
    }

    #[test]
    fn 多バイトのディレクトリでも開始位置は文字単位() {
        // 「ソース/日本語.rs」→ ディレクトリ 3 文字 + `/` なのでファイル名は 4 文字目から。
        let (directory, name, start) = split_relative("ソース/日本語.rs");
        assert_eq!(directory, "ソース");
        assert_eq!(name, "日本語.rs");
        assert_eq!(start, 4);
    }

    #[test]
    fn 強調位置をファイル名側へ写せる() {
        // "ソース/日本語.rs" の 4,5 文字目 (「日」「本」) を強調する。
        let shifted = shift_positions(&[0, 4, 5], 4, 5);
        assert_eq!(shifted, vec![0, 1]);
    }
}
