//! エディタ領域。タブとペイン分割を管理し、開いているバッファを並べる。

use crate::actions;
use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{metrics, theme};
use crate::ui::{empty_state, h_flex, v_flex};
use crate::views::editor_view::{EditorView, EditorViewEvent};
use crate::views::markdown_preview::MarkdownPreviewView;
use gpui::prelude::*;
use gpui::{App, Context, Entity, EventEmitter, Subscription, Window, div, px};
use nebula_protocol::{
    BufferId, Event, NotificationLevel, Position, Request, Response, WorkspaceId, WorkspaceInfo,
};
use std::path::PathBuf;

pub enum EditorAreaEvent {
    Notify(NotificationLevel, String),
    OpenFile {
        path: PathBuf,
        position: Option<Position>,
    },
}

/// 1 タブ。
struct Tab {
    buffer_id: BufferId,
    path: Option<PathBuf>,
    title: String,
    view: Entity<EditorView>,
    dirty: bool,
    /// Markdown プレビュー。ペイン分割とは別物 (タブに従属する表示) なので、
    /// ペインではなくタブごとに持つ。markdown 以外のタブでも作ってはおくが
    /// (作成自体は軽い)、`preview_visible` が立っていて言語が markdown の
    /// ときだけ [`EditorArea::render_pane`] が実際に描画へ組み込む。
    preview: Entity<MarkdownPreviewView>,
    /// プレビューを表示中か。タブごとに覚える (同じファイルでもタブが別なら別々)。
    preview_visible: bool,
    /// タブごとの購読。タブを閉じると一緒に解放される。
    _subscription: Subscription,
}

impl Tab {
    /// アクティブなタブの言語が markdown かどうか。プレビューをタブバーに
    /// 出すかどうか・実際に描画へ組み込むかどうかの両方がこれ 1 箇所に依る。
    fn is_markdown(&self, cx: &App) -> bool {
        self.view.read(cx).config().language.as_deref() == Some("markdown")
    }
}

/// 1 ペイン。タブの並びと選択状態を持つ。
#[derive(Default)]
struct Pane {
    tabs: Vec<Tab>,
    active: usize,
}

impl Pane {
    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }
}

/// 並び (タブの列、ペインの列など) から要素を1つ取り除いたあと、
/// 選択位置がどこを指すべきかを計算する。
///
/// タブを閉じても「見ていたはずのもの」がなるべく変わらないようにしたい:
/// 閉じた位置より手前の選択はそのまま、閉じた位置より後ろの選択は
/// 要素がひとつ詰まった分だけ手前にずれる。選択していた要素自体を閉じた場合は
/// 同じ位置（詰まった結果、右隣だったものが来る）に留まり、それが末尾を超えるなら
/// 新しい末尾に留まる。並びが空になった場合は 0 を返す（呼び出し側で空扱いする）。
fn index_after_removal(removed: usize, old_selected: usize, new_len: usize) -> usize {
    if new_len == 0 {
        return 0;
    }
    if removed < old_selected {
        old_selected - 1
    } else if removed > old_selected {
        old_selected
    } else {
        removed.min(new_len - 1)
    }
}

pub struct EditorArea {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    panes: Vec<Pane>,
    active_pane: usize,
}

impl EventEmitter<EditorAreaEvent> for EditorArea {}

impl EditorArea {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            client: None,
            workspace: None,
            panes: vec![Pane::default()],
            active_pane: 0,
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        cx.notify();
    }

    /// バックエンドからのイベントを、関係するタブへ配る。
    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        match event {
            Event::Diagnostics { path, diagnostics } => {
                for pane in &self.panes {
                    for tab in &pane.tabs {
                        if tab.path.as_deref() == Some(path.as_path()) {
                            let diagnostics = diagnostics.clone();
                            tab.view
                                .update(cx, |view, cx| view.set_diagnostics(diagnostics, cx));
                        }
                    }
                }
            }
            Event::BufferFileChanged {
                buffer,
                new_text,
                version,
            } => {
                for pane in &self.panes {
                    for tab in &pane.tabs {
                        if tab.buffer_id == *buffer {
                            let text = new_text.clone();
                            let version = *version;
                            tab.view
                                .update(cx, |view, cx| view.reload(&text, version, cx));
                            // reload は EditorViewEvent::Dirtied を出さない (自分自身への
                            // notify だけ) ので、on_editor_event 側の転送に乗れない。
                            // ディスク上の変更 (他プロセスでの編集・git checkout など) で
                            // プレビューが古いまま固まらないよう、ここでも明示的に notify する。
                            tab.preview.update(cx, |_, cx| cx.notify());
                        }
                    }
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// ファイルを開く。既に開いていればそのタブを選ぶ。
    pub fn open_file(
        &mut self,
        workspace: WorkspaceId,
        path: PathBuf,
        position: Option<Position>,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane_index, tab_index)) = self.find_tab_by_path(&path) {
            self.active_pane = pane_index;
            self.panes[pane_index].active = tab_index;
            if let Some(position) = position {
                let view = self.panes[pane_index].tabs[tab_index].view.clone();
                view.update(cx, |view, cx| view.reveal_position(position, cx));
            }
            cx.notify();
            return;
        }

        let Some(client) = self.client.clone() else {
            cx.emit(EditorAreaEvent::Notify(
                NotificationLevel::Warning,
                "バックエンドに接続していません".into(),
            ));
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::OpenBuffer {
                    workspace,
                    path: path.clone(),
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(Response::Buffer(snapshot)) => {
                    this.add_tab(workspace, snapshot, position, cx);
                }
                Ok(_) => {}
                Err(e) => cx.emit(EditorAreaEvent::Notify(
                    NotificationLevel::Error,
                    format!("{} を開けません: {e}", path.display()),
                )),
            })
            .ok();
        })
        .detach();
    }

    fn add_tab(
        &mut self,
        workspace: WorkspaceId,
        snapshot: nebula_protocol::BufferSnapshot,
        position: Option<Position>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let title = snapshot
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "無題".to_string());
        let buffer_id = snapshot.id;
        let path = snapshot.path.clone();
        let view = cx.new(|cx| EditorView::new(client, workspace, snapshot, cx));
        let subscription = cx.subscribe(&view, Self::on_editor_event);
        let preview = cx.new(|_cx| MarkdownPreviewView::new(view.clone()));

        if let Some(position) = position {
            view.update(cx, |view, cx| view.reveal_position(position, cx));
        }

        let pane = &mut self.panes[self.active_pane];
        pane.tabs.push(Tab {
            buffer_id,
            path,
            title,
            view,
            dirty: false,
            preview,
            preview_visible: false,
            _subscription: subscription,
        });
        pane.active = pane.tabs.len() - 1;
        cx.notify();
    }

    fn on_editor_event(
        &mut self,
        entity: Entity<EditorView>,
        event: &EditorViewEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorViewEvent::Dirtied | EditorViewEvent::Saved => {
                let dirty = entity.read(cx).is_dirty();
                let buffer_id = entity.read(cx).buffer_id;
                for pane in &mut self.panes {
                    for tab in &mut pane.tabs {
                        if tab.buffer_id == buffer_id {
                            tab.dirty = dirty;
                            // 新しい通知機構は作らず、この既存の購読に乗る。
                            // プレビューは埋め込まれてさえいれば親 (EditorArea) の
                            // 再描画にぶら下がって再変換されるはずだが、それに
                            // 賭けず自分自身にも notify しておくことで、GPUI の
                            // 再描画伝播の仕方に依らず確実に追従させる。
                            tab.preview.update(cx, |_, cx| cx.notify());
                        }
                    }
                }
                cx.notify();
            }
            EditorViewEvent::Notify(level, message) => {
                cx.emit(EditorAreaEvent::Notify(*level, message.clone()));
            }
            EditorViewEvent::OpenFile { path, position } => {
                cx.emit(EditorAreaEvent::OpenFile {
                    path: path.clone(),
                    position: *position,
                });
            }
        }
    }

    fn find_tab_by_path(&self, path: &PathBuf) -> Option<(usize, usize)> {
        for (pane_index, pane) in self.panes.iter().enumerate() {
            for (tab_index, tab) in pane.tabs.iter().enumerate() {
                if tab.path.as_ref() == Some(path) {
                    return Some((pane_index, tab_index));
                }
            }
        }
        None
    }

    /// コマンドパレットから呼ばれる操作。
    pub fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        match command {
            "editor.save" => self.save_active(cx),
            "editor.close" => self.close_active(cx),
            "editor.splitRight" => self.split_right(cx),
            "editor.nextTab" => self.cycle_tab(1, cx),
            "editor.previousTab" => self.cycle_tab(-1, cx),
            "editor.togglePreview" => self.toggle_preview(cx),
            _ => {}
        }
    }

    fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.panes[self.active_pane].active_tab() else {
            return;
        };
        let view = tab.view.clone();
        view.update(cx, |view, cx| view.save(cx));
    }

    /// アクティブなタブを閉じる。コマンドパレット・ショートカットからはここに来る。
    fn close_active(&mut self, cx: &mut Context<Self>) {
        let pane_index = self.active_pane;
        let index = self.panes[pane_index].active;
        self.close_tab(pane_index, index, cx);
    }

    /// 指定した位置のタブを閉じる。タブの ✖ ボタンは自分がアクティブでなくても
    /// 押せるため、「今アクティブなタブ」ではなく pane_index/index を明示的に受け取る。
    fn close_tab(&mut self, pane_index: usize, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.panes[pane_index].tabs.get(index) else {
            return;
        };
        let buffer_id = tab.buffer_id;
        let old_active = self.panes[pane_index].active;
        self.panes[pane_index].tabs.remove(index);
        let len = self.panes[pane_index].tabs.len();
        self.panes[pane_index].active = index_after_removal(index, old_active, len);

        // 分割していて空になったペインは畳む。空の枠が残ると場所を無駄にする。
        if len == 0 && self.panes.len() > 1 {
            let old_active_pane = self.active_pane;
            self.panes.remove(pane_index);
            self.active_pane = index_after_removal(pane_index, old_active_pane, self.panes.len());
        }

        if let Some(client) = self.client.clone() {
            cx.spawn(async move |_this, _cx| {
                let _ = client
                    .request(Request::CloseBuffer { buffer: buffer_id })
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    fn cycle_tab(&mut self, delta: i32, cx: &mut Context<Self>) {
        let pane = &mut self.panes[self.active_pane];
        if pane.tabs.is_empty() {
            return;
        }
        let len = pane.tabs.len() as i32;
        pane.active = (((pane.active as i32 + delta) % len + len) % len) as usize;
        cx.notify();
    }

    fn split_right(&mut self, cx: &mut Context<Self>) {
        // 2 分割までに留める。3 つ以上はタブ幅が実用的でなくなる。
        if self.panes.len() >= 2 {
            return;
        }
        self.panes.push(Pane::default());
        self.active_pane = self.panes.len() - 1;
        cx.notify();
    }

    /// アクティブなタブのプレビュー表示を切り替える。markdown 以外のタブで
    /// 呼ばれても (ショートカット経由などで) 何もしない。実際に表示するかどうかは
    /// `render_pane` が `Tab::is_markdown` と合わせて判断するので、ここでは
    /// フラグを立てるだけでよい。
    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        let pane = &mut self.panes[self.active_pane];
        let Some(tab) = pane.tabs.get_mut(pane.active) else {
            return;
        };
        tab.preview_visible = !tab.preview_visible;
        cx.notify();
    }

    // -- アクション --

    fn on_save(&mut self, _: &actions::Save, _w: &mut Window, cx: &mut Context<Self>) {
        self.save_active(cx);
    }
    fn on_close_tab(&mut self, _: &actions::CloseTab, _w: &mut Window, cx: &mut Context<Self>) {
        self.close_active(cx);
    }
    fn on_next_tab(&mut self, _: &actions::NextTab, _w: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, cx);
    }
    fn on_previous_tab(
        &mut self,
        _: &actions::PreviousTab,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_tab(-1, cx);
    }
    fn on_split_right(&mut self, _: &actions::SplitRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.split_right(cx);
    }
    fn on_toggle_preview(
        &mut self,
        _: &actions::TogglePreview,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_preview(cx);
    }

    fn on_new_file(&mut self, _: &actions::NewFile, _w: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.clone()) else {
            cx.emit(EditorAreaEvent::Notify(
                NotificationLevel::Warning,
                "フォルダを開いてから新規ファイルを作成してください".into(),
            ));
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::CreateScratchBuffer {
                    workspace: workspace.id,
                    language: None,
                })
                .await;
            this.update(cx, |this, cx| {
                if let Ok(Response::Buffer(snapshot)) = result {
                    this.add_tab(workspace.id, snapshot, None, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_save_as(&mut self, _: &actions::SaveAs, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.panes[self.active_pane].active_tab() else {
            return;
        };
        let view = tab.view.clone();
        let directory = self
            .workspace
            .as_ref()
            .map(|w| w.root.clone())
            .unwrap_or_else(|| std::env::temp_dir());
        let prompt = cx.prompt_for_new_path(&directory, None);
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(Some(path))) = prompt.await {
                view.update(cx, |view, cx| view.save_as(path, cx)).ok();
            }
        })
        .detach();
    }

    // -- 描画 --

    fn render_pane(&self, pane_index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let pane = &self.panes[pane_index];
        let is_active_pane = pane_index == self.active_pane;
        // プレビュー切り替えボタンはタブバーに 1 つだけ (アクティブなタブが
        // markdown のときだけ)。表示するかどうかもこの 2 つの値だけで決まる。
        let active_tab_markdown = pane.active_tab().is_some_and(|tab| tab.is_markdown(cx));
        let preview_visible = pane.active_tab().is_some_and(|tab| tab.preview_visible);

        let tabs: Vec<_> = pane
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let selected = index == pane.active;
                let title = tab.title.clone();
                let dirty = tab.dirty;
                h_flex()
                    .id(("tab", pane_index * 1000 + index))
                    .h_full()
                    .px(px(12.))
                    .gap(px(7.))
                    .flex_none()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(theme.border)
                    .text_size(px(12.))
                    .when(selected, |el| {
                        el.bg(theme.editor_bg)
                            .text_color(theme.text)
                            // 選択タブの上端にネオンの線を引き、どこを見ているか一目で分かるようにする。
                            .border_t_2()
                            .border_color(theme.accent)
                    })
                    .when(!selected, |el| {
                        el.text_color(theme.text_faint)
                            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.text_muted))
                    })
                    .child(title)
                    // 未保存マークは装飾として残す。閉じるボタンとは別物なので、
                    // これがあっても ✖ は常に押せる（未保存タブも ✖ から閉じられる）。
                    .when(dirty, |el| {
                        el.child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(theme.accent_secondary),
                        )
                    })
                    .child(
                        crate::ui::icon_button(
                            ("tab-close", pane_index * 1000 + index),
                            Icon::Close,
                            false,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            // ✖ 自体の click_listener はタブ本体の on_click と別の
                            // hitbox で独立して発火し、放っておくと親（タブ本体）の
                            // on_click にもバブリングして active を上書きしてしまう
                            // （削除でずれた古い index が入り、タブ数と active が
                            // 不整合になる）ので、必ず先に伝播を止める。
                            cx.stop_propagation();
                            this.close_tab(pane_index, index, cx);
                        })),
                    )
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.active_pane = pane_index;
                        this.panes[pane_index].active = index;
                        cx.notify();
                    }))
            })
            .collect();

        // プレビューはペイン分割 (`split_right`) とは別物: 「もう1つのペイン」では
        // なくアクティブなタブに従属する表示なので、2 ペイン上限の枠組みには
        // 触れず、ここで body の隣に直接並べる。
        let content = match pane.active_tab() {
            Some(tab) if active_tab_markdown && preview_visible => h_flex()
                .flex_1()
                .h_full()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .overflow_hidden()
                        .child(tab.view.clone().into_any_element()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .overflow_hidden()
                        .border_l_1()
                        .border_color(theme.border)
                        .child(tab.preview.clone().into_any_element()),
                )
                .into_any_element(),
            Some(tab) => div()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .child(tab.view.clone().into_any_element())
                .into_any_element(),
            None => div()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .child(empty_state(
                    "ファイルが開かれていません\n⌘P でクイックオープン、⌘⇧P でコマンドパレット",
                    cx,
                ))
                .into_any_element(),
        };

        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .when(is_active_pane && self.panes.len() > 1, |el| {
                el.border_l_1().border_color(theme.border_glow)
            })
            .child(
                h_flex()
                    .h(metrics::TAB_HEIGHT)
                    .w_full()
                    .flex_none()
                    .bg(theme.bg_elevated)
                    .border_b_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .children(tabs)
                    .child(div().flex_1())
                    .when(active_tab_markdown, |el| {
                        el.child(
                            crate::ui::icon_button(
                                ("preview-toggle", pane_index),
                                Icon::Eye,
                                preview_visible,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _w, cx| this.toggle_preview(cx))),
                        )
                    })
                    .child(
                        crate::ui::icon_button(
                            ("split", pane_index),
                            Icon::Split,
                            false,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _w, cx| this.split_right(cx))),
                    ),
            )
            .child(content)
            .into_any_element()
    }
}

impl Render for EditorArea {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panes: Vec<_> = (0..self.panes.len())
            .map(|index| self.render_pane(index, cx))
            .collect();
        div()
            .size_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_toggle_preview))
            .on_action(cx.listener(Self::on_new_file))
            .on_action(cx.listener(Self::on_save_as))
            .children(panes)
    }
}

#[cfg(test)]
mod tests {
    use super::index_after_removal;

    #[test]
    fn 三枚のうち真ん中を閉じたときアクティブが先頭にあれば動かない() {
        // [0,1,2] の 1 を閉じる。アクティブは 0（閉じた位置より前）。
        assert_eq!(index_after_removal(1, 0, 2), 0);
    }

    #[test]
    fn 三枚のうち真ん中を閉じたときアクティブがそこだったら詰まった右隣に移る() {
        // [0,1,2] の 1 を閉じる。アクティブも 1（閉じたタブ自身）。
        // 削除後は元の 2 が位置 1 に詰まってくるので、そこがアクティブになる。
        assert_eq!(index_after_removal(1, 1, 2), 1);
    }

    #[test]
    fn 三枚のうち真ん中を閉じたときアクティブが後ろにあれば1つ前にずれる() {
        // [0,1,2] の 1 を閉じる。アクティブは 2（閉じた位置より後ろ）。
        assert_eq!(index_after_removal(1, 2, 2), 1);
    }

    #[test]
    fn アクティブなタブ自身を閉じたとき末尾なら新しい末尾に留まる() {
        // [0,1,2] の 2（末尾）を閉じる。アクティブも 2。
        // 詰めた結果の末尾は 1 なので、そこにクランプされる。
        assert_eq!(index_after_removal(2, 2, 2), 1);
    }

    #[test]
    fn 最後の1枚を閉じたときは空扱いで0になる() {
        // [0] の 0 を閉じる。アクティブも 0。要素が無くなるので 0（呼び出し側が空を判断する）。
        assert_eq!(index_after_removal(0, 0, 0), 0);
    }

    #[test]
    fn アクティブより後ろのタブを閉じたときアクティブは動かない() {
        // [0,1,2,3] の 3（末尾）を閉じる。アクティブは 1（閉じた位置より前）。
        assert_eq!(index_after_removal(3, 1, 3), 1);
    }

    #[test]
    fn アクティブより前のタブを閉じたときアクティブは1つ減る() {
        // [0,1,2,3] の 0（先頭）を閉じる。アクティブは 2（閉じた位置より後ろ）。
        assert_eq!(index_after_removal(0, 2, 3), 1);
    }
}
