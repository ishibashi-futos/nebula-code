//! チャットの状態遷移。
//!
//! [`ChatItem`] と、それを操作する純粋関数だけを集めてある。GPUI に依存しないので
//! ウィンドウを起動せずに単体テストできる ([`super`] のモジュール doc の設計要点 3 を参照)。

use nebula_protocol::{
    CodexApprovalDecision, CodexApprovalRequest, CodexConversationId, CodexEvent,
};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// 会話の要素
// ---------------------------------------------------------------------------

/// 会話に並ぶ 1 要素。
///
/// `sealed` は「この吹き出しはもう伸びない」印。ターン完了か確定メッセージの受信で立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChatItem {
    User(String),
    Assistant {
        text: String,
        sealed: bool,
    },
    Reasoning {
        text: String,
        sealed: bool,
        collapsed: bool,
    },
    Exec {
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        output: String,
        exit_code: Option<i32>,
    },
    Patch {
        files: Vec<PathBuf>,
    },
    Approval {
        request: CodexApprovalRequest,
        decision: Option<CodexApprovalDecision>,
    },
    Error(String),
    /// セッション確立などの短い案内。
    Notice(String),
    Raw {
        method: String,
        payload: String,
        expanded: bool,
    },
}

/// まだ伸びている途中の吹き出しを探す。
///
/// 末尾だけを見ないのは、バックエンドが翻訳できない通知を [`ChatItem::Raw`] として
/// 素通しするため。増分の合間に 1 つ挟まるだけで末尾が変わり、続きが別の吹き出しへ
/// 分かれたり、確定文が二重に出たりする。
///
/// 探索は直前の [`ChatItem::User`] で打ち切る。中断などで封をされないまま残った
/// 前ターンの吹き出しに、次のターンの応答を書き足さないため。
fn open_bubble(items: &[ChatItem], is_open: fn(&ChatItem) -> bool) -> Option<usize> {
    for (index, item) in items.iter().enumerate().rev() {
        if matches!(item, ChatItem::User(_)) {
            return None;
        }
        if is_open(item) {
            return Some(index);
        }
    }
    None
}

fn is_open_assistant(item: &ChatItem) -> bool {
    matches!(item, ChatItem::Assistant { sealed: false, .. })
}

fn is_open_reasoning(item: &ChatItem) -> bool {
    matches!(item, ChatItem::Reasoning { sealed: false, .. })
}

/// アシスタント応答の増分を足す。封のされた吹き出しには足さない。
pub(super) fn push_assistant_delta(items: &mut Vec<ChatItem>, delta: &str) {
    match open_bubble(items, is_open_assistant).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Assistant { text, .. }) => text.push_str(delta),
        _ => items.push(ChatItem::Assistant {
            text: delta.to_string(),
            sealed: false,
        }),
    }
}

/// 確定したアシスタント応答。増分で組み立てた本文を全文で置き換える。
///
/// 置き換えるのは、増分と確定文の両方が届く実装でも二重に出さないため。
pub(super) fn finish_assistant_message(items: &mut Vec<ChatItem>, full: String) {
    match open_bubble(items, is_open_assistant).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Assistant { text, sealed }) => {
            *text = full;
            *sealed = true;
        }
        _ => items.push(ChatItem::Assistant {
            text: full,
            sealed: true,
        }),
    }
}

pub(super) fn push_reasoning_delta(items: &mut Vec<ChatItem>, delta: &str) {
    match open_bubble(items, is_open_reasoning).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Reasoning { text, .. }) => text.push_str(delta),
        _ => items.push(ChatItem::Reasoning {
            text: delta.to_string(),
            sealed: false,
            collapsed: false,
        }),
    }
}

/// ターンの区切り。以降の増分は新しい吹き出しになる。
///
/// 推論はここで畳む。読み終わったターンの思考過程が開いたままだと、
/// 会話を遡るときに本文が埋もれる。
pub(super) fn seal_turn(items: &mut [ChatItem]) {
    for item in items.iter_mut() {
        match item {
            ChatItem::Assistant { sealed, .. } => *sealed = true,
            ChatItem::Reasoning {
                sealed, collapsed, ..
            } => {
                if !*sealed {
                    *collapsed = true;
                }
                *sealed = true;
            }
            _ => {}
        }
    }
}

pub(super) fn begin_exec(
    items: &mut Vec<ChatItem>,
    call_id: String,
    command: Vec<String>,
    cwd: PathBuf,
) {
    items.push(ChatItem::Exec {
        call_id,
        command,
        cwd,
        output: String::new(),
        exit_code: None,
    });
}

/// 実行中コマンドを `call_id` で探す。
///
/// 「末尾の要素」ではなく ID で引くのは、複数のコマンドが並行して走ることがあるため。
fn find_exec<'a>(items: &'a mut [ChatItem], call_id: &str) -> Option<&'a mut ChatItem> {
    items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, ChatItem::Exec { call_id: id, .. } if id == call_id))
}

pub(super) fn append_exec_output(items: &mut [ChatItem], call_id: &str, chunk: &str) {
    if let Some(ChatItem::Exec { output, .. }) = find_exec(items, call_id) {
        output.push_str(chunk);
    }
}

pub(super) fn finish_exec(items: &mut [ChatItem], call_id: &str, code: i32) {
    if let Some(ChatItem::Exec { exit_code, .. }) = find_exec(items, call_id) {
        *exit_code = Some(code);
    }
}

/// 承認カードに結果を書き込む。既に応答済みなら何もしない (二重送信を防ぐ)。
pub(super) fn resolve_approval(
    items: &mut [ChatItem],
    request_id: &str,
    answer: CodexApprovalDecision,
) -> bool {
    for item in items.iter_mut().rev() {
        if let ChatItem::Approval { request, decision } = item
            && request.request_id == request_id
        {
            if decision.is_some() {
                return false;
            }
            *decision = Some(answer);
            return true;
        }
    }
    false
}

/// イベントが属する会話。`None` はどの会話にも紐づかない生イベント。
pub(super) fn conversation_of(event: &CodexEvent) -> Option<CodexConversationId> {
    match event {
        CodexEvent::SessionConfigured { conversation, .. }
        | CodexEvent::AgentMessageDelta { conversation, .. }
        | CodexEvent::AgentMessage { conversation, .. }
        | CodexEvent::ReasoningDelta { conversation, .. }
        | CodexEvent::ExecBegin { conversation, .. }
        | CodexEvent::ExecOutput { conversation, .. }
        | CodexEvent::ExecEnd { conversation, .. }
        | CodexEvent::PatchApplied { conversation, .. }
        | CodexEvent::ApprovalRequested { conversation, .. }
        | CodexEvent::TurnComplete { conversation, .. }
        | CodexEvent::Error { conversation, .. } => Some(*conversation),
        CodexEvent::Raw { conversation, .. } => *conversation,
    }
}

/// 未応答の承認が残っているか。残っていると Codex 側の処理は止まったままになる。
pub(super) fn has_pending_approval(items: &[ChatItem]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, ChatItem::Approval { decision: None, .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::CodexApprovalKind;

    fn approval(request_id: &str) -> CodexApprovalRequest {
        CodexApprovalRequest {
            request_id: request_id.to_string(),
            kind: CodexApprovalKind::ExecCommand,
            summary: "rm -rf /tmp/x を実行します".into(),
            detail: "rm -rf /tmp/x".into(),
        }
    }

    #[test]
    fn 増分は同じ吹き出しに追記される() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        push_assistant_delta(&mut items, "にちは");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn 確定メッセージは増分を全文で置き換える() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        finish_assistant_message(&mut items, "こんにちは".into());
        assert_eq!(items.len(), 1, "吹き出しは増えない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: true
            }
        );
    }

    fn raw() -> ChatItem {
        ChatItem::Raw {
            method: "item/started".into(),
            payload: "{}".into(),
            expanded: false,
        }
    }

    #[test]
    fn 増分の合間に生イベントが挟まっても同じ吹き出しに続く() {
        // バックエンドは翻訳できない通知を Raw で素通しする。末尾だけを見ていると
        // ここで吹き出しが割れる。
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        items.push(raw());
        push_assistant_delta(&mut items, "にちは");
        assert_eq!(items.len(), 2, "吹き出しは割れない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn 生イベントを挟んだ確定メッセージも二重に出ない() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        items.push(raw());
        finish_assistant_message(&mut items, "こんにちは".into());
        assert_eq!(items.len(), 2, "確定文が別の吹き出しになっていない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: true
            }
        );
    }

    #[test]
    fn 推論も生イベントを跨いで続く() {
        let mut items = Vec::new();
        push_reasoning_delta(&mut items, "考え");
        items.push(raw());
        push_reasoning_delta(&mut items, "中");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            ChatItem::Reasoning {
                text: "考え中".into(),
                sealed: false,
                collapsed: false
            }
        );
    }

    #[test]
    fn 封をされていない前ターンの吹き出しには書き足さない() {
        // 中断でターンが終わると封がされないまま残る。次のターンの応答が
        // そこへ吸い込まれると、送信した順に会話が読めなくなる。
        let mut items = vec![
            ChatItem::Assistant {
                text: "中断された応答".into(),
                sealed: false,
            },
            ChatItem::User("次の質問".into()),
        ];
        push_assistant_delta(&mut items, "新しい応答");
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "中断された応答".into(),
                sealed: false
            }
        );
        assert_eq!(
            items[2],
            ChatItem::Assistant {
                text: "新しい応答".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn ターン完了後の増分は新しい吹き出しになる() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "1 回目");
        seal_turn(&mut items);
        push_assistant_delta(&mut items, "2 回目");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[1],
            ChatItem::Assistant {
                text: "2 回目".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn ターン完了で推論が畳まれる() {
        let mut items = Vec::new();
        push_reasoning_delta(&mut items, "考え中");
        seal_turn(&mut items);
        assert_eq!(
            items[0],
            ChatItem::Reasoning {
                text: "考え中".into(),
                sealed: true,
                collapsed: true
            }
        );
    }

    #[test]
    fn コマンド出力は_call_id_ごとに振り分けられる() {
        let mut items = Vec::new();
        begin_exec(
            &mut items,
            "a".into(),
            vec!["ls".into()],
            PathBuf::from("/"),
        );
        begin_exec(
            &mut items,
            "b".into(),
            vec!["pwd".into()],
            PathBuf::from("/"),
        );
        append_exec_output(&mut items, "a", "file\n");
        append_exec_output(&mut items, "b", "/\n");
        finish_exec(&mut items, "a", 0);
        finish_exec(&mut items, "b", 1);

        assert_eq!(
            items[0],
            ChatItem::Exec {
                call_id: "a".into(),
                command: vec!["ls".into()],
                cwd: PathBuf::from("/"),
                output: "file\n".into(),
                exit_code: Some(0),
            }
        );
        assert_eq!(
            items[1],
            ChatItem::Exec {
                call_id: "b".into(),
                command: vec!["pwd".into()],
                cwd: PathBuf::from("/"),
                output: "/\n".into(),
                exit_code: Some(1),
            }
        );
    }

    #[test]
    fn 未知の_call_id_の出力は捨てる() {
        let mut items = vec![ChatItem::Notice("x".into())];
        append_exec_output(&mut items, "none", "出力");
        assert_eq!(items, vec![ChatItem::Notice("x".into())]);
    }

    #[test]
    fn 承認は一度だけ応答できる() {
        let mut items = vec![ChatItem::Approval {
            request: approval("r1"),
            decision: None,
        }];
        assert!(has_pending_approval(&items));
        assert!(resolve_approval(
            &mut items,
            "r1",
            CodexApprovalDecision::Approve
        ));
        assert!(!resolve_approval(
            &mut items,
            "r1",
            CodexApprovalDecision::Deny
        ));
        assert!(!has_pending_approval(&items));
        assert_eq!(
            items[0],
            ChatItem::Approval {
                request: approval("r1"),
                decision: Some(CodexApprovalDecision::Approve),
            }
        );
    }

    #[test]
    fn 存在しない承認への応答は無視される() {
        let mut items = Vec::new();
        assert!(!resolve_approval(
            &mut items,
            "none",
            CodexApprovalDecision::Approve
        ));
    }
}
