//! Git パネル (ソース管理)。
//!
//! バックエンドが `git` CLI を叩き、結果は `GitRepoStatus` として届く。
//! このビューが持つのはその写しと、まだ送っていない入力 (コミットメッセージ・
//! 新しいブランチ名) だけ。ステージやコミットのような副作用のある操作は、
//! バックエンドが完了後に `Event::GitStatusChanged` を配ってくれるので、
//! **応答を見て自分で一覧を作り直すことはしない**。二重に更新経路を持つと、
//! 外部で `git` を実行されたときだけ表示がずれる、という追いにくいバグになる。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{Theme, theme};
use crate::ui::format_keystroke;
use crate::ui::{
    TextInput, empty_state, h_flex, icon, icon_button, list_row, nebula_accent_line, panel_header,
    primary_button, simple_tooltip, tooltip_text, truncate_middle, v_flex,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, Hsla, MouseButton, SharedString,
    Stateful, Subscription, Window, div, px, uniform_list,
};
use nebula_protocol::{
    GitBranch, GitFileStatus, GitRepoStatus, GitStatusCode, NotificationLevel, WorkspaceInfo,
};
use std::ops::Range;
use std::path::PathBuf;

mod actions;
mod rows;

use rows::{GitRow, ROW_HEIGHT, checkout_name, format_sync_counts, split_path_display};

// `code_for`/`status_char`/`status_color`/`GitSection` は `crate::views::git::` 直下の
// 公開パスとしてエクスプローラー側から参照されている。分割後もそのパスを保つため、
// ここで `rows` から再エクスポートする。
pub(crate) use rows::{GitSection, code_for, status_char, status_color};

pub enum GitEvent {
    OpenFile(PathBuf),
    Notify(NotificationLevel, String),
}

// ---------------------------------------------------------------------------
// Git パネル本体
// ---------------------------------------------------------------------------

/// 実行中の遠隔操作。実行中は同じ操作を重ねて出さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Push,
    Pull,
    Commit,
    Checkout,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Push => "プッシュ中…",
            Busy::Pull => "プル中…",
            Busy::Commit => "コミット中…",
            Busy::Checkout => "切り替え中…",
        }
    }
}

pub struct GitView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    status: GitRepoStatus,
    /// `status` から作った表示用の平坦な一覧。描画のたびに組み直さないよう保持する。
    rows: Vec<GitRow>,
    collapsed: Vec<GitSection>,
    branches: Vec<GitBranch>,
    branch_menu_open: bool,
    creating_branch: bool,
    amend: bool,
    /// 破棄の確認待ち (絶対パス, 表示名)。
    discard: Option<(PathBuf, String)>,
    busy: Option<Busy>,
    hovered: Option<usize>,
    commit_input: Entity<TextInput>,
    branch_input: Entity<TextInput>,
    /// 保持しないと購読が即座に解除される。
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GitEvent> for GitView {}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 一覧の行に置く小さなアイコンボタン。共通の `icon_button` は 28px あり、
/// 24px の行に収まらないのでここだけ小型のものを使う。
fn row_icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    color: Hsla,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .justify_center()
        .size(px(18.))
        .rounded(px(4.))
        .child(icon(glyph, px(11.), color))
        .hover(|s| s.bg(theme.bg_surface))
        .cursor_pointer()
}

/// 見出しに置く文字だけの小さなボタン。
fn mini_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .h(px(16.))
        .px(px(5.))
        .rounded(px(3.))
        .text_size(px(10.))
        .text_color(theme.accent)
        .bg(theme.accent_soft)
        .cursor_pointer()
        .hover(|s| s.text_color(theme.text))
        .child(label.into())
}

impl GitView {
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let branch = self
            .status
            .branch
            .clone()
            .unwrap_or_else(|| "(切り離された HEAD)".to_string());
        let counts = format_sync_counts(self.status.ahead, self.status.behind);
        let busy = self.busy;

        h_flex()
            .h(px(36.))
            .px(px(8.))
            .gap(px(6.))
            .flex_none()
            .justify_between()
            .child(
                h_flex()
                    .id("git-branch")
                    .h(px(24.))
                    .px(px(6.))
                    .gap(px(5.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if self.branch_menu_open {
                        theme.border_glow
                    } else {
                        theme.border
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_overlay))
                    .child(icon(Icon::Branch, px(12.), theme.accent))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(truncate_middle(&branch, 20)),
                    )
                    .when(!counts.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.accent_secondary)
                                .child(counts.clone()),
                        )
                    })
                    .child(icon(Icon::ChevronDown, px(10.), theme.text_faint))
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_branch_menu(cx)))
                    .tooltip(simple_tooltip(tooltip_text::GIT_SWITCH_BRANCH)),
            )
            .child(match busy {
                Some(busy) => h_flex()
                    .h(px(20.))
                    .px(px(8.))
                    .rounded(px(10.))
                    .bg(theme.accent_soft)
                    .text_size(px(10.5))
                    .text_color(theme.accent)
                    .child(busy.label())
                    .into_any_element(),
                // 行の破棄ボタンが Undo を使うので、プルには双方向矢印の Refresh を割り当てる。
                // 同じ形の記号を別の意味で 2 か所に出すと押し間違えるため。
                None => h_flex()
                    .gap(px(2.))
                    .child(
                        icon_button("git-pull", Icon::Refresh, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.sync(false, cx)))
                            .tooltip(simple_tooltip(tooltip_text::GIT_PULL)),
                    )
                    .child(
                        icon_button("git-push", Icon::Send, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.sync(true, cx)))
                            .tooltip(simple_tooltip(tooltip_text::GIT_PUSH)),
                    )
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_commit_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let staged = self.staged_count();
        // amend は既存コミットの書き換えなので、ステージ済みが 0 でも意味がある。
        let enabled = self.busy.is_none() && (staged > 0 || self.amend);
        let label = if self.amend {
            "修正してコミット"
        } else {
            "コミット"
        };

        v_flex()
            .px(px(8.))
            .pb(px(8.))
            .gap(px(6.))
            .flex_none()
            .child(
                div()
                    .p(px(6.))
                    .rounded(px(6.))
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .text_color(theme.text)
                    .cursor(gpui::CursorStyle::IBeam)
                    // 枠の余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = self.commit_input.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(self.commit_input.clone()),
            )
            .child(
                h_flex()
                    .justify_between()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .id("git-amend")
                            .gap(px(5.))
                            .h(px(20.))
                            .px(px(5.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(
                                h_flex()
                                    .justify_center()
                                    .size(px(13.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(if self.amend {
                                        theme.accent
                                    } else {
                                        theme.border_strong
                                    })
                                    .when(self.amend, |el| {
                                        el.bg(theme.accent_soft).child(icon(
                                            Icon::Check,
                                            px(9.),
                                            theme.accent,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(if self.amend {
                                        theme.accent
                                    } else {
                                        theme.text_muted
                                    })
                                    .child("変更を修正 (amend)"),
                            )
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.amend = !this.amend;
                                cx.notify();
                            })),
                    )
                    .child(
                        primary_button("git-commit", label, enabled, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.commit(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            return div()
                .flex_1()
                .child(empty_state("変更はありません", cx))
                .into_any_element();
        }
        let count = self.rows.len();
        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "git-rows",
                    count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range.map(|index| this.render_row(index, cx)).collect()
                    }),
                )
                .h_full(),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(index) {
            Some(GitRow::Header {
                section,
                count,
                collapsed,
            }) => self.render_section_header(index, *section, *count, *collapsed, cx),
            Some(GitRow::File { section, entry }) => {
                self.render_file_row(index, *section, entry.clone(), cx)
            }
            None => div().h(ROW_HEIGHT).into_any_element(),
        }
    }

    fn render_section_header(
        &self,
        index: usize,
        section: GitSection,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let hovered = self.hovered == Some(index);
        h_flex()
            .id(("git-row", index))
            .w_full()
            .h(ROW_HEIGHT)
            .px(px(8.))
            .gap(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_overlay))
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                this.hover_row(index, *hovered, cx);
            }))
            .on_click(cx.listener(move |this, _, _w, cx| this.toggle_section(section, cx)))
            .child(icon(
                if collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronDown
                },
                px(10.),
                theme.text_faint,
            ))
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(section.title()),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(count.to_string()),
            )
            .child(div().flex_1())
            .when(hovered, |el| {
                el.child(
                    mini_button(("git-bulk", index), section.bulk_label(), &theme).on_click(
                        cx.listener(move |this, _, _w, cx| {
                            cx.stop_propagation();
                            this.bulk(section, cx);
                        }),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_file_row(
        &self,
        index: usize,
        section: GitSection,
        entry: GitFileStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let hovered = self.hovered == Some(index);
        let code = code_for(&entry, section);
        let (name, parent) = split_path_display(&entry.path);
        let absolute = self.absolute(&entry.path);
        let display_name = truncate_middle(&name, 26);
        let display_parent = truncate_middle(&parent, 22);

        list_row(("git-row", index), false, cx)
            .h(ROW_HEIGHT)
            .gap(px(5.))
            .overflow_hidden()
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                this.hover_row(index, *hovered, cx);
            }))
            .on_click({
                let absolute = absolute.clone();
                cx.listener(move |_this, _, _w, cx| {
                    cx.emit(GitEvent::OpenFile(absolute.clone()));
                })
            })
            .child(
                div()
                    .w(px(11.))
                    .flex_none()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(status_color(code, &theme))
                    .child(status_char(code).to_string()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(if code == GitStatusCode::Deleted {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(display_name),
            )
            .when(!display_parent.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .overflow_hidden()
                        .child(display_parent),
                )
            })
            .child(div().flex_1())
            .when(hovered, |el| {
                el.child(self.render_row_actions(index, section, absolute, name, cx))
            })
            .into_any_element()
    }

    /// 行のホバーで出す操作ボタン。親のクリック (ファイルを開く) に流さないよう
    /// いずれも伝播を止める。
    fn render_row_actions(
        &self,
        index: usize,
        section: GitSection,
        absolute: PathBuf,
        name: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let mut row = h_flex().gap(px(1.)).flex_none();

        if section != GitSection::Staged {
            let target = absolute.clone();
            row = row.child(
                row_icon_button(
                    ("git-discard", index),
                    Icon::Undo,
                    theme.git_deleted,
                    &theme,
                )
                .on_click(cx.listener(move |this, _, _w, cx| {
                    cx.stop_propagation();
                    this.discard = Some((target.clone(), name.clone()));
                    cx.notify();
                }))
                .tooltip(simple_tooltip(tooltip_text::GIT_DISCARD)),
            );
        }
        row = match section {
            GitSection::Staged => {
                let target = absolute.clone();
                row.child(
                    row_icon_button(
                        ("git-unstage", index),
                        Icon::Close,
                        theme.text_muted,
                        &theme,
                    )
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        cx.stop_propagation();
                        this.unstage(vec![target.clone()], cx);
                    }))
                    .tooltip(simple_tooltip(tooltip_text::GIT_UNSTAGE)),
                )
            }
            _ => {
                let target = absolute.clone();
                row.child(
                    row_icon_button(("git-stage", index), Icon::Plus, theme.accent, &theme)
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            cx.stop_propagation();
                            this.stage(vec![target.clone()], cx);
                        }))
                        .tooltip(simple_tooltip(tooltip_text::GIT_STAGE)),
                )
            }
        };
        row.into_any_element()
    }

    fn render_branch_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.branch_menu_open {
            return None;
        }
        let theme = theme(cx).clone();
        let branches = self.branches.clone();
        let count = branches.len();
        // 一覧の高さは行数に合わせるが、上限を設けてパネルを覆い尽くさないようにする。
        let list_height = px((count.clamp(1, 9) as f32) * 24.);

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                // 背後を覆う板。ここを押すと閉じる。
                .child(
                    div()
                        .id("git-branch-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_click(cx.listener(|this, _, _w, cx| {
                            this.branch_menu_open = false;
                            cx.notify();
                        })),
                )
                .child(
                    v_flex()
                        .absolute()
                        // パネル見出し (32) + アクセント線 (1) + ヘッダ内のブランチ表示の下端。
                        .top(px(66.))
                        .left(px(8.))
                        .right(px(8.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.border_glow)
                        .overflow_hidden()
                        .child(nebula_accent_line(theme.accent))
                        .child(
                            h_flex()
                                .id("git-new-branch")
                                .h(px(26.))
                                .px(px(8.))
                                .gap(px(5.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.bg_surface))
                                .child(icon(Icon::Plus, px(11.), theme.accent))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.accent)
                                        .child("新しいブランチ"),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.creating_branch = true;
                                    this.branch_input.read(cx).focus(window);
                                    cx.notify();
                                })),
                        )
                        .when(self.creating_branch, |el| {
                            el.child(
                                v_flex()
                                    .px(px(8.))
                                    .pb(px(6.))
                                    .gap(px(3.))
                                    .child(
                                        div()
                                            .px(px(5.))
                                            .py(px(3.))
                                            .rounded(px(4.))
                                            .bg(theme.bg_surface)
                                            .border_1()
                                            .border_color(theme.border_glow)
                                            .overflow_hidden()
                                            .text_size(px(12.))
                                            .line_height(px(17.))
                                            .text_color(theme.text)
                                            .cursor(gpui::CursorStyle::IBeam)
                                            // 枠の余白を押しても欄へ入れるようにする。
                                            .on_mouse_down(MouseButton::Left, {
                                                let input = self.branch_input.clone();
                                                move |_, window, cx| input.read(cx).focus(window)
                                            })
                                            .child(self.branch_input.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(theme.text_faint)
                                            .child(format!(
                                                "{} で作成して切り替え / {} で取消",
                                                format_keystroke("enter"),
                                                format_keystroke("escape")
                                            )),
                                    ),
                            )
                        })
                        .child(div().h(px(1.)).w_full().bg(theme.border))
                        .child(if count == 0 {
                            div()
                                .h(px(26.))
                                .px(px(8.))
                                .text_size(px(11.))
                                .text_color(theme.text_faint)
                                .child("ブランチを読み込んでいます…")
                                .into_any_element()
                        } else {
                            div()
                                .h(list_height)
                                .child(
                                    uniform_list(
                                        "git-branches",
                                        count,
                                        cx.processor(
                                            move |this, range: Range<usize>, _window, cx| {
                                                range
                                                    .map(|index| this.render_branch_row(index, cx))
                                                    .collect()
                                            },
                                        ),
                                    )
                                    .h_full(),
                                )
                                .into_any_element()
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_branch_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let Some(branch) = self.branches.get(index) else {
            return div().h(px(24.)).into_any_element();
        };
        let name = branch.name.clone();
        let summary = branch.last_commit_summary.clone();
        let is_head = branch.is_head;
        let is_remote = branch.is_remote;
        let target = checkout_name(branch);

        h_flex()
            .id(("git-branch-row", index))
            .w_full()
            .h(px(24.))
            .px(px(8.))
            .gap(px(5.))
            .cursor_pointer()
            .overflow_hidden()
            .hover(|s| s.bg(theme.bg_surface))
            .when(is_head, |el| el.bg(theme.accent_soft))
            .child(div().w(px(11.)).flex_none().when(is_head, |el| {
                el.child(icon(Icon::Check, px(10.), theme.accent))
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(if is_remote {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(truncate_middle(&name, 24)),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(truncate_middle(&summary, 24)),
            )
            .on_click(cx.listener(move |this, _, _w, cx| {
                if is_head {
                    this.branch_menu_open = false;
                    cx.notify();
                    return;
                }
                this.checkout(target.clone(), false, cx);
            }))
            .into_any_element()
    }

    fn render_discard_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, name) = self.discard.as_ref()?;
        let theme = theme(cx).clone();
        let name = name.clone();
        Some(
            v_flex()
                .id("git-discard-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .items_center()
                .justify_center()
                .p(px(12.))
                .bg(theme.bg_void.opacity(0.75))
                // 暗くした余白を押しても下のファイル行に届かせない。確認中に
                // 別のファイルが開くと、どれを破棄しようとしていたか分からなくなる。
                .on_click(|_, _w, cx| cx.stop_propagation())
                .child(
                    v_flex()
                        .w_full()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.git_deleted)
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text)
                                .child(format!(
                                    "{} の変更を破棄しますか?",
                                    truncate_middle(&name, 24)
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("この操作は取り消せません。"),
                        )
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .justify_end()
                                .child(
                                    h_flex()
                                        .id("git-discard-cancel")
                                        .h(px(24.))
                                        .px(px(10.))
                                        .justify_center()
                                        .rounded(px(5.))
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted)
                                        .border_1()
                                        .border_color(theme.border)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.bg_surface))
                                        .child("やめる")
                                        .on_click(cx.listener(|this, _, _w, cx| {
                                            this.discard = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    h_flex()
                                        .id("git-discard-ok")
                                        .h(px(24.))
                                        .px(px(10.))
                                        .justify_center()
                                        .rounded(px(5.))
                                        .text_size(px(11.5))
                                        .text_color(theme.text_inverse)
                                        .bg(theme.git_deleted)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.error))
                                        .child("破棄する")
                                        .on_click(
                                            cx.listener(|this, _, _w, cx| this.confirm_discard(cx)),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for GitView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();

        if self.repo_root().is_none() {
            return v_flex()
                .size_full()
                .child(panel_header("ソース管理", cx))
                .child(empty_state(
                    "このフォルダは git リポジトリではありません。\n\
                     git init を実行するか、管理下のフォルダを開いてください。",
                    cx,
                ))
                .into_any_element();
        }

        let progress = self.status.in_progress.clone();

        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(panel_header("ソース管理", cx))
            .child(nebula_accent_line(theme.accent_soft))
            .child(self.render_header(cx))
            .when_some(progress, |el, progress| {
                // リベース中・マージ中は操作の意味が変わるので、常に目に入る位置に出す。
                el.child(
                    h_flex()
                        .h(px(20.))
                        .px(px(8.))
                        .flex_none()
                        .bg(theme.accent_soft)
                        .text_size(px(10.5))
                        .text_color(theme.accent_secondary)
                        .child(format!("{progress} が進行中")),
                )
            })
            .child(self.render_commit_box(cx))
            .child(self.render_list(cx))
            .children(self.render_branch_menu(cx))
            .children(self.render_discard_confirm(cx))
            .into_any_element()
    }
}
