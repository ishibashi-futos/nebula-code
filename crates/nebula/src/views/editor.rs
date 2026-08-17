//! エディタ領域。タブとペイン分割を管理し、開いているバッファを並べる。

use crate::actions;
use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{metrics, theme};
use crate::ui::{empty_state, h_flex, icon, v_flex};
use crate::views::editor_view::{EditorView, EditorViewEvent};
use gpui::prelude::*;
use gpui::{Context, Entity, EventEmitter, Subscription, Window, div, px};
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
    /// タブごとの購読。タブを閉じると一緒に解放される。
    _subscription: Subscription,
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

    fn close_active(&mut self, cx: &mut Context<Self>) {
        let pane_index = self.active_pane;
        let Some(tab) = self.panes[pane_index]
            .tabs
            .get(self.panes[pane_index].active)
        else {
            return;
        };
        let buffer_id = tab.buffer_id;
        let index = self.panes[pane_index].active;
        self.panes[pane_index].tabs.remove(index);
        let len = self.panes[pane_index].tabs.len();
        self.panes[pane_index].active = index.min(len.saturating_sub(1));

        // 分割していて空になったペインは畳む。空の枠が残ると場所を無駄にする。
        if len == 0 && self.panes.len() > 1 {
            self.panes.remove(pane_index);
            self.active_pane = self.active_pane.min(self.panes.len() - 1);
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
                    .child(if dirty {
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(theme.accent_secondary)
                            .into_any_element()
                    } else {
                        icon(Icon::Close, px(11.), theme.text_faint).into_any_element()
                    })
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.active_pane = pane_index;
                        this.panes[pane_index].active = index;
                        cx.notify();
                    }))
            })
            .collect();

        let body = match pane.active_tab() {
            Some(tab) => tab.view.clone().into_any_element(),
            None => empty_state(
                "ファイルが開かれていません\n⌘P でクイックオープン、⌘⇧P でコマンドパレット",
                cx,
            )
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
            .child(div().flex_1().overflow_hidden().child(body))
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
            .on_action(cx.listener(Self::on_new_file))
            .on_action(cx.listener(Self::on_save_as))
            .children(panes)
    }
}
