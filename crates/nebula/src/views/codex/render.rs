//! チャット行・空状態・入力欄・承認 UI の描画。
//!
//! [`CodexView`] の状態を読むだけで、状態そのものは持たない。ヘッダやドロップダウンの
//! 描画はルート ([`super`]) に残し、会話本文の描画だけをここへ切り出してある。

use super::CodexView;
use super::chat::{ChatItem, has_pending_approval};
use super::format::{
    MarkdownSegment, approval_kind_label, decision_index, decision_label, diff_line_color,
    format_command, split_code_blocks,
};
use crate::assets::Icon;
use crate::theme::{metrics, theme};
use crate::ui::format_keystroke;
use crate::ui::{h_flex, icon, truncate_middle, v_flex};
use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Hsla, MouseButton, div, px, relative};
use nebula_protocol::{CodexApprovalDecision, CodexApprovalKind, CodexApprovalRequest};
use std::path::{Path, PathBuf};

/// 等幅で出すブロックの書体。書体名は theme.rs の metrics に集約してある。
const MONO_FONT: &str = metrics::MONO_FONT_FAMILY;

impl CodexView {
    pub(super) fn render_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        if self.items.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p(px(20.))
                .gap(px(8.))
                .child(icon(Icon::Sparkles, px(26.), theme.accent_tertiary))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .text_center()
                        .child("最初のメッセージを送ると会話が始まります"),
                )
                .into_any_element();
        }

        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| self.render_item(index, item, cx))
            .collect();

        v_flex()
            .id("codex-messages")
            .flex_1()
            // flex 子要素は既定で内容ぶんの高さを主張するため、下限を 0 にしないと
            // スクロールせずパネルを押し広げてしまう。
            .min_h(px(0.))
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(8.))
            .pb(px(6.))
            .gap(px(8.))
            .children(rows)
            .into_any_element()
    }

    fn render_item(&self, index: usize, item: &ChatItem, cx: &mut Context<Self>) -> AnyElement {
        match item {
            ChatItem::User(text) => self.render_user(text, cx),
            ChatItem::Assistant { text, .. } => self.render_assistant(index, text, cx),
            ChatItem::Reasoning {
                text, collapsed, ..
            } => self.render_reasoning(index, text, *collapsed, cx),
            ChatItem::Exec {
                command,
                cwd,
                output,
                exit_code,
                ..
            } => self.render_exec(command, cwd, output, *exit_code, cx),
            ChatItem::Patch { files } => self.render_patch(files, cx),
            ChatItem::Approval { request, decision } => {
                self.render_approval(index, request, *decision, cx)
            }
            ChatItem::Error(message) => self.render_error(message, cx),
            ChatItem::Notice(message) => self.render_notice(message, cx),
            ChatItem::Raw {
                method,
                payload,
                expanded,
            } => self.render_raw(index, method, payload, *expanded, cx),
        }
    }

    fn render_user(&self, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.88))
                    .px(px(9.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(theme.bg_overlay)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(text.to_string()),
            )
            .into_any_element()
    }

    fn render_assistant(&self, index: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let blocks: Vec<AnyElement> = split_code_blocks(text)
            .into_iter()
            .enumerate()
            .map(|(part, segment)| match segment {
                MarkdownSegment::Text(body) => div()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(body)
                    .into_any_element(),
                MarkdownSegment::Code { language, body } => {
                    self.render_code_block(("code", index * 64 + part), language, body, cx)
                }
            })
            .collect();

        v_flex()
            .w_full()
            .gap(px(5.))
            .children(blocks)
            .into_any_element()
    }

    fn render_code_block(
        &self,
        id: impl Into<ElementId>,
        language: Option<String>,
        body: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .id(id)
            .w_full()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(theme.bg_void)
            .border_1()
            .border_color(theme.border)
            .when_some(language, |el, language| {
                el.child(
                    div()
                        .px(px(7.))
                        .py(px(2.))
                        .text_size(px(9.5))
                        .text_color(theme.accent_tertiary)
                        .bg(theme.bg_surface)
                        .child(language),
                )
            })
            .child(
                div()
                    .px(px(7.))
                    .py(px(5.))
                    .font_family(MONO_FONT)
                    .text_size(px(11.))
                    .text_color(theme.text)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_reasoning(
        &self,
        index: usize,
        text: &str,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .gap(px(2.))
            .child(
                h_flex()
                    .id(("reasoning", index))
                    .gap(px(4.))
                    .cursor_pointer()
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
                            .text_color(theme.text_faint)
                            .child("推論"),
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        if let Some(ChatItem::Reasoning { collapsed, .. }) =
                            this.items.get_mut(index)
                        {
                            *collapsed = !*collapsed;
                            cx.notify();
                        }
                    })),
            )
            .when(!collapsed, |el| {
                el.child(
                    div()
                        .pl(px(14.))
                        .italic()
                        .text_size(px(11.))
                        .text_color(theme.text_faint)
                        .child(text.to_string()),
                )
            })
            .into_any_element()
    }

    fn render_exec(
        &self,
        command: &[String],
        cwd: &Path,
        output: &str,
        exit_code: Option<i32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let status_color = match exit_code {
            Some(0) => theme.success,
            Some(_) => theme.error,
            None => theme.accent,
        };
        let status = match exit_code {
            Some(0) => "完了".to_string(),
            Some(code) => format!("終了コード {code}"),
            None => "実行中…".to_string(),
        };

        v_flex()
            .w_full()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(theme.bg_void)
            .border_1()
            .border_color(if exit_code.is_some_and(|c| c != 0) {
                theme.error
            } else {
                theme.border
            })
            .child(
                h_flex()
                    .w_full()
                    .px(px(7.))
                    .py(px(4.))
                    .gap(px(5.))
                    .bg(theme.bg_surface)
                    .child(icon(Icon::Terminal, px(11.), status_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(MONO_FONT)
                            .text_size(px(11.))
                            .text_color(theme.text)
                            .child(format_command(command)),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(status_color)
                            .child(status),
                    ),
            )
            .child(
                div()
                    .px(px(7.))
                    .py(px(2.))
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .child(truncate_middle(&cwd.display().to_string(), 42)),
            )
            .when(!output.trim().is_empty(), |el| {
                el.child(
                    div()
                        .px(px(7.))
                        .py(px(5.))
                        .font_family(MONO_FONT)
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(output.trim_end().to_string()),
                )
            })
            .into_any_element()
    }

    fn render_patch(&self, files: &[PathBuf], cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .p(px(7.))
            .gap(px(3.))
            .rounded(px(5.))
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.git_modified)
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(icon(Icon::Edit, px(11.), theme.git_modified))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.git_modified)
                            .child(format!("{} 件のファイルを変更", files.len())),
                    ),
            )
            .children(files.iter().map(|path| {
                div()
                    .font_family(MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(truncate_middle(&path.display().to_string(), 44))
            }))
            .into_any_element()
    }

    /// 承認カード。ネオンの枠と余白で会話中のどの要素より目立たせる。
    ///
    /// 見落とすと Codex 側の処理が止まったままになるため、ここだけは主張を強くする。
    fn render_approval(
        &self,
        index: usize,
        request: &CodexApprovalRequest,
        decision: Option<CodexApprovalDecision>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let pending = decision.is_none();
        let detail_lines: Vec<AnyElement> = request
            .detail
            .lines()
            .take(60)
            .map(|line| {
                let color = match request.kind {
                    CodexApprovalKind::ApplyPatch => diff_line_color(line, &theme),
                    _ => theme.text,
                };
                div()
                    .font_family(MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(color)
                    .child(line.to_string())
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w_full()
            .p(px(9.))
            .gap(px(6.))
            .rounded(px(7.))
            .bg(theme.bg_overlay)
            .border_2()
            .border_color(if pending {
                theme.border_glow
            } else {
                theme.border
            })
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(icon(
                        Icon::Warning,
                        px(13.),
                        if pending {
                            theme.accent_secondary
                        } else {
                            theme.text_faint
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if pending {
                                theme.accent_secondary
                            } else {
                                theme.text_faint
                            })
                            .child(approval_kind_label(request.kind)),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(request.summary.clone()),
            )
            .when(!detail_lines.is_empty(), |el| {
                el.child(
                    v_flex()
                        .w_full()
                        .p(px(6.))
                        .rounded(px(5.))
                        .bg(theme.bg_void)
                        .children(detail_lines),
                )
            })
            .child(match decision {
                Some(decision) => h_flex()
                    .gap(px(5.))
                    .child(icon(Icon::Check, px(11.), theme.text_muted))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(decision_label(decision)),
                    )
                    .into_any_element(),
                None => h_flex()
                    .gap(px(5.))
                    .flex_wrap()
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Approve,
                        "許可",
                        theme.success,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::ApproveForSession,
                        "常に許可",
                        theme.accent,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Deny,
                        "拒否",
                        theme.warning,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Abort,
                        "中止",
                        theme.error,
                        cx,
                    ))
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_approval_button(
        &self,
        index: usize,
        decision: CodexApprovalDecision,
        label: &'static str,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let request_id = match self.items.get(index) {
            Some(ChatItem::Approval { request, .. }) => request.request_id.clone(),
            _ => return div().into_any_element(),
        };
        h_flex()
            .id(("approval-btn", index * 8 + decision_index(decision)))
            .h(px(24.))
            .px(px(9.))
            .justify_center()
            .rounded(px(5.))
            .border_1()
            .border_color(color)
            .text_size(px(11.))
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_surface))
            .child(label)
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.respond_approval(request_id.clone(), decision, cx)
            }))
            .into_any_element()
    }

    fn render_error(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .items_start()
            .gap(px(5.))
            .p(px(7.))
            .rounded(px(5.))
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.error)
            .child(icon(Icon::Error, px(12.), theme.error))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(11.5))
                    .text_color(theme.error)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    fn render_notice(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .justify_center()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    fn render_raw(
        &self,
        index: usize,
        method: &str,
        payload: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .gap(px(2.))
            .child(
                h_flex()
                    .id(("raw", index))
                    .gap(px(4.))
                    .cursor_pointer()
                    .child(icon(
                        if expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        px(9.),
                        theme.text_faint,
                    ))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(format!("raw: {method}")),
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        if let Some(ChatItem::Raw { expanded, .. }) = this.items.get_mut(index) {
                            *expanded = !*expanded;
                            cx.notify();
                        }
                    })),
            )
            .when(expanded, |el| {
                el.child(
                    div()
                        .pl(px(13.))
                        .font_family(MONO_FONT)
                        .text_size(px(9.5))
                        .text_color(theme.text_faint)
                        .child(payload.to_string()),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let focused = self.input.read(cx).is_focused();
        let can_send = !self.input.read(cx).text().trim().is_empty();
        let waiting = has_pending_approval(&self.items);

        v_flex()
            .flex_none()
            .w_full()
            .p(px(8.))
            .gap(px(6.))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.bg_elevated)
            .when(waiting, |el| {
                // 承認カードが画面外へ流れても気づけるよう、入力欄の直上でも知らせる。
                el.child(
                    h_flex()
                        .w_full()
                        .gap(px(5.))
                        .px(px(7.))
                        .py(px(4.))
                        .rounded(px(5.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.accent_secondary)
                        .child(icon(Icon::Warning, px(11.), theme.accent_secondary))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.accent_secondary)
                                .child("承認待ちです"),
                        ),
                )
            })
            .child(
                div()
                    .w_full()
                    .px(px(7.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(if focused {
                        theme.border_glow
                    } else {
                        theme.border
                    })
                    .overflow_hidden()
                    .text_size(metrics::UI_FONT_SIZE)
                    .line_height(px(
                        f32::from(metrics::UI_FONT_SIZE) * metrics::LINE_HEIGHT_RATIO
                    ))
                    .text_color(theme.text)
                    .cursor(gpui::CursorStyle::IBeam)
                    // 枠の余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = self.input.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(self.input.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "{} 送信 / {} 改行",
                                format_keystroke("enter"),
                                format_keystroke("shift-enter")
                            )),
                    )
                    .child(if self.running {
                        h_flex()
                            .id("codex-stop")
                            .h(px(24.))
                            .px(px(9.))
                            .gap(px(4.))
                            .justify_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.error)
                            .text_size(px(11.))
                            .text_color(theme.error)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(icon(Icon::Stop, px(10.), theme.error))
                            .child("停止")
                            .on_click(cx.listener(|this, _e, _w, cx| this.interrupt(cx)))
                            .into_any_element()
                    } else {
                        h_flex()
                            .id("codex-send")
                            .h(px(24.))
                            .px(px(10.))
                            .gap(px(4.))
                            .justify_center()
                            .rounded(px(5.))
                            .text_size(px(11.))
                            .when(can_send, |el| {
                                el.bg(theme.accent_tertiary)
                                    .text_color(theme.text_inverse)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.accent))
                            })
                            .when(!can_send, |el| {
                                el.bg(theme.bg_overlay).text_color(theme.text_faint)
                            })
                            .child(icon(
                                Icon::Send,
                                px(10.),
                                if can_send {
                                    theme.text_inverse
                                } else {
                                    theme.text_faint
                                },
                            ))
                            .child("送信")
                            .on_click(cx.listener(|this, _e, _w, cx| this.submit(cx)))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }
}
