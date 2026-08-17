//! app-server のプロトコルを Nebula の語彙へ翻訳する純粋関数群。
//!
//! codex のプロトコルは版ごとに項目が増減する。ここを副作用の無い関数に切り出して
//! おくことで、採取した実際の 1 行をそのまま食わせて回帰を検出できる。
//! 翻訳できなかったものは握り潰さず `CodexEvent::Raw` として GUI まで通す。

use nebula_protocol::{
    CodexApprovalDecision, CodexApprovalKind, CodexApprovalRequest, CodexConversationId,
    CodexEvent, CodexSandboxPolicy, CodexTokenUsage,
};
use nebula_protocol::{CodexApprovalPolicy, CodexSessionSpec};
use serde_json::{Value, json};
use std::path::PathBuf;

/// コマンド実行の承認要求。
pub const EXEC_APPROVAL: &str = "item/commandExecution/requestApproval";
/// ファイル変更の承認要求。
pub const PATCH_APPROVAL: &str = "item/fileChange/requestApproval";

/// 通知の `threadId`。会話 ID の解決に使う。
pub fn thread_id(params: &Value) -> Option<&str> {
    params.get("threadId").and_then(Value::as_str)
}

/// サーバー通知を 1 つの `CodexEvent` に翻訳する。
///
/// `conversation` は呼び出し側が `threadId` から解決した会話 ID。解決できない通知
/// (アカウント関連など会話に紐づかないもの) は Raw で通す。
/// `token_usage` は `thread/tokenUsage/updated` で控えておいた最新値で、
/// ターン完了イベントに載せる。
pub fn translate_notification(
    method: &str,
    params: &Value,
    conversation: Option<CodexConversationId>,
    token_usage: Option<CodexTokenUsage>,
) -> CodexEvent {
    let Some(conversation) = conversation else {
        return raw(None, method, params);
    };
    known_event(method, params, conversation, token_usage)
        .unwrap_or_else(|| raw(Some(conversation), method, params))
}

fn known_event(
    method: &str,
    params: &Value,
    conversation: CodexConversationId,
    token_usage: Option<CodexTokenUsage>,
) -> Option<CodexEvent> {
    let event = match method {
        "item/agentMessage/delta" => CodexEvent::AgentMessageDelta {
            conversation,
            delta: text(params, "delta")?,
        },
        // 推論には要約 (summary) と本文 (text) の 2 系統があるが、GUI では
        // どちらも「考え中の表示」に流し込むので区別しない。
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            CodexEvent::ReasoningDelta {
                conversation,
                delta: text(params, "delta")?,
            }
        }
        "item/commandExecution/outputDelta" => CodexEvent::ExecOutput {
            conversation,
            call_id: text(params, "itemId")?,
            chunk: text(params, "delta")?,
        },
        "item/started" => item_started(params.get("item")?, conversation)?,
        "item/completed" => item_completed(params.get("item")?, conversation)?,
        "turn/completed" => CodexEvent::TurnComplete {
            conversation,
            token_usage,
        },
        "error" => CodexEvent::Error {
            conversation,
            message: text(params.get("error")?, "message")?,
        },
        _ => return None,
    };
    Some(event)
}

fn item_started(item: &Value, conversation: CodexConversationId) -> Option<CodexEvent> {
    match text(item, "type")?.as_str() {
        "commandExecution" => Some(CodexEvent::ExecBegin {
            conversation,
            call_id: text(item, "id")?,
            // app-server はシェルへ渡す 1 本の文字列で寄越す。同梱の commandActions は
            // 「best-effort な解析結果」と明記されているので argv の復元には使わない。
            command: vec![text(item, "command")?],
            cwd: PathBuf::from(text(item, "cwd")?),
        }),
        _ => None,
    }
}

fn item_completed(item: &Value, conversation: CodexConversationId) -> Option<CodexEvent> {
    match text(item, "type")?.as_str() {
        "agentMessage" => Some(CodexEvent::AgentMessage {
            conversation,
            text: text(item, "text")?,
        }),
        "commandExecution" => Some(CodexEvent::ExecEnd {
            conversation,
            call_id: text(item, "id")?,
            // 中断されたコマンドは exitCode が null で届く。終了コードなしを
            // 表す値が protocol 側に無いため -1 に寄せる。
            exit_code: item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32,
        }),
        // 適用前 (inProgress) や失敗した変更を「適用済み」として流すと GUI が
        // 実ファイルと食い違うため、完了したものだけを PatchApplied にする。
        "fileChange" if text(item, "status")?.as_str() == "completed" => {
            Some(CodexEvent::PatchApplied {
                conversation,
                files: item
                    .get("changes")?
                    .as_array()?
                    .iter()
                    .filter_map(|change| text(change, "path"))
                    .map(PathBuf::from)
                    .collect(),
            })
        }
        _ => None,
    }
}

/// `thread/tokenUsage/updated` から会話全体の累計使用量を取り出す。
///
/// `last` は直近 1 リクエストぶんなので、会話の消費量を出したい GUI には
/// `total` を渡す。
pub fn parse_token_usage(params: &Value) -> Option<CodexTokenUsage> {
    let total = params.get("tokenUsage")?.get("total")?;
    Some(CodexTokenUsage {
        input_tokens: count(total, "inputTokens"),
        cached_input_tokens: count(total, "cachedInputTokens"),
        output_tokens: count(total, "outputTokens"),
        total_tokens: count(total, "totalTokens"),
    })
}

/// `item/started` (fileChange) から (項目 ID, unified diff) を取り出す。
///
/// ファイル変更の承認要求 (`item/fileChange/requestApproval`) の params には
/// itemId しか入っておらず diff が無い。承認 UI に差分を見せるため、先に流れてくる
/// item/started の内容をここで文字列化して控えておく。
pub fn patch_preview(item: &Value) -> Option<(String, String)> {
    if text(item, "type")?.as_str() != "fileChange" {
        return None;
    }
    let id = text(item, "id")?;
    let diff = item
        .get("changes")?
        .as_array()?
        .iter()
        .filter_map(|change| {
            Some(format!(
                "--- {}\n{}",
                text(change, "path")?,
                text(change, "diff")?
            ))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((id, diff))
}

/// サーバーからの承認要求を GUI 向けの形に翻訳する。承認要求でなければ `None`。
///
/// `patch_preview_text` は `patch_preview` で控えておいた差分。
pub fn translate_approval(
    method: &str,
    params: &Value,
    request_id: &str,
    patch_preview_text: Option<String>,
) -> Option<CodexApprovalRequest> {
    let reason = text(params, "reason");
    let (kind, default_summary, detail) = match method {
        EXEC_APPROVAL => (
            CodexApprovalKind::ExecCommand,
            "コマンドの実行を許可しますか",
            text(params, "command").unwrap_or_default(),
        ),
        PATCH_APPROVAL => (
            CodexApprovalKind::ApplyPatch,
            "ファイル変更の適用を許可しますか",
            patch_preview_text.unwrap_or_default(),
        ),
        _ => return None,
    };
    Some(CodexApprovalRequest {
        request_id: request_id.to_string(),
        kind,
        summary: reason.unwrap_or_else(|| default_summary.to_string()),
        detail,
    })
}

/// 承認の判断を app-server の応答 result に変換する。
///
/// コマンド承認とファイル変更承認は決定の文字列が共通なので 1 つの関数で足りる。
pub fn decision_result(decision: CodexApprovalDecision) -> Value {
    let decision = match decision {
        CodexApprovalDecision::Approve => "accept",
        CodexApprovalDecision::ApproveForSession => "acceptForSession",
        // decline は「この操作だけ拒否してターンは続行」、cancel は「ターンごと中断」。
        CodexApprovalDecision::Deny => "decline",
        CodexApprovalDecision::Abort => "cancel",
    };
    json!({ "decision": decision })
}

/// 会話開始要求 (`thread/start`) のパラメータを組み立てる。
pub fn thread_start_params(spec: &CodexSessionSpec) -> Value {
    json!({
        "cwd": spec.cwd.as_ref().map(|p| p.display().to_string()),
        "model": spec.model,
        "approvalPolicy": approval_policy(spec.approval_policy),
        "sandbox": sandbox_mode(spec.sandbox_policy),
    })
}

/// codex 0.147.0 の AskForApproval は untrusted / on-request / never の 3 値しかない。
/// Nebula の `OnFailure` に対応する値が無いため、確認を求める側に倒して
/// `on-request` にする。
fn approval_policy(policy: CodexApprovalPolicy) -> &'static str {
    match policy {
        CodexApprovalPolicy::Untrusted => "untrusted",
        CodexApprovalPolicy::OnFailure | CodexApprovalPolicy::OnRequest => "on-request",
        CodexApprovalPolicy::Never => "never",
    }
}

fn sandbox_mode(policy: CodexSandboxPolicy) -> &'static str {
    match policy {
        CodexSandboxPolicy::ReadOnly => "read-only",
        CodexSandboxPolicy::WorkspaceWrite => "workspace-write",
        CodexSandboxPolicy::DangerFullAccess => "danger-full-access",
    }
}

/// 添付ファイルを `turn/start` の入力項目にする。
///
/// ファイル本文を本文へ埋め込むのではなく、スキーマにある専用項目を使う。
/// 画像は `localImage`、それ以外は `mention` (エディタでの @ 参照と同じ扱い) で、
/// 巨大なファイルを本文に流し込んでコンテキストを潰さずに済む。
pub fn attachment_input(path: &std::path::Path) -> Value {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let path_text = path.display().to_string();
    if is_image(path) {
        json!({ "type": "localImage", "path": path_text })
    } else {
        json!({ "type": "mention", "name": name, "path": path_text })
    }
}

fn is_image(path: &std::path::Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

fn raw(conversation: Option<CodexConversationId>, method: &str, params: &Value) -> CodexEvent {
    CodexEvent::Raw {
        conversation,
        method: method.to_string(),
        payload: params.to_string(),
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION: CodexConversationId = CodexConversationId(1);

    /// 実際に `codex app-server` を動かして採取した通知の params 部分。
    fn params(json_text: &str) -> Value {
        serde_json::from_str(json_text).expect("採取した JSON の解析")
    }

    fn translate(method: &str, json_text: &str) -> CodexEvent {
        translate_notification(method, &params(json_text), Some(CONVERSATION), None)
    }

    #[test]
    fn 応答の増分を翻訳する() {
        let event = translate(
            "item/agentMessage/delta",
            r#"{"threadId":"019fedc4","turnId":"019fedc4-0d79","itemId":"msg_0ec2","delta":" run"}"#,
        );
        assert_eq!(
            event,
            CodexEvent::AgentMessageDelta {
                conversation: CONVERSATION,
                delta: " run".to_string(),
            }
        );
    }

    #[test]
    fn 応答の完了を翻訳する() {
        let event = translate(
            "item/completed",
            r#"{"item":{"type":"agentMessage","id":"msg_0ec2","text":"DONE","phase":"final_answer","memoryCitation":null},"threadId":"019fedc4","turnId":"019fedc4-0d79","completedAtMs":1786400491182}"#,
        );
        assert_eq!(
            event,
            CodexEvent::AgentMessage {
                conversation: CONVERSATION,
                text: "DONE".to_string(),
            }
        );
    }

    #[test]
    fn 推論の増分は要約と本文の両方を拾う() {
        for method in [
            "item/reasoning/summaryTextDelta",
            "item/reasoning/textDelta",
        ] {
            let event = translate(
                method,
                r#"{"threadId":"019fedc4","turnId":"t","itemId":"rs_0","summaryIndex":0,"contentIndex":0,"delta":"考え中"}"#,
            );
            assert_eq!(
                event,
                CodexEvent::ReasoningDelta {
                    conversation: CONVERSATION,
                    delta: "考え中".to_string(),
                }
            );
        }
    }

    #[test]
    fn コマンド実行の開始を翻訳する() {
        let event = translate(
            "item/started",
            r#"{"item":{"type":"commandExecution","id":"exec-10720fe5","pluginId":null,"scriptPath":null,"command":"/bin/zsh -lc 'echo hello-nebula'","cwd":"/tmp/nebula-codex-test","processId":"32235","source":"unifiedExecStartup","status":"inProgress","commandActions":[{"type":"unknown","command":"echo hello-nebula"}],"aggregatedOutput":null,"exitCode":null,"durationMs":null},"threadId":"019fedc4","turnId":"t","startedAtMs":1786400484302}"#,
        );
        assert_eq!(
            event,
            CodexEvent::ExecBegin {
                conversation: CONVERSATION,
                call_id: "exec-10720fe5".to_string(),
                command: vec!["/bin/zsh -lc 'echo hello-nebula'".to_string()],
                cwd: PathBuf::from("/tmp/nebula-codex-test"),
            }
        );
    }

    #[test]
    fn コマンド実行の終了を翻訳する() {
        let event = translate(
            "item/completed",
            r#"{"item":{"type":"commandExecution","id":"exec-10720fe5","command":"/bin/zsh -lc 'echo hello-nebula'","cwd":"/tmp/nebula-codex-test","status":"completed","commandActions":[],"aggregatedOutput":"hello-nebula\n","exitCode":0,"durationMs":0},"threadId":"019fedc4","turnId":"t","completedAtMs":1786400484302}"#,
        );
        assert_eq!(
            event,
            CodexEvent::ExecEnd {
                conversation: CONVERSATION,
                call_id: "exec-10720fe5".to_string(),
                exit_code: 0,
            }
        );
    }

    #[test]
    fn 終了コードが無いコマンドは負値で表す() {
        let event = translate(
            "item/completed",
            r#"{"item":{"type":"commandExecution","id":"exec-1","command":"sleep 100","cwd":"/tmp","status":"failed","exitCode":null},"threadId":"019fedc4","turnId":"t"}"#,
        );
        assert_eq!(
            event,
            CodexEvent::ExecEnd {
                conversation: CONVERSATION,
                call_id: "exec-1".to_string(),
                exit_code: -1,
            }
        );
    }

    #[test]
    fn コマンド出力の増分を翻訳する() {
        let event = translate(
            "item/commandExecution/outputDelta",
            r#"{"threadId":"019fedc4","turnId":"t","itemId":"exec-1","delta":"hello\n"}"#,
        );
        assert_eq!(
            event,
            CodexEvent::ExecOutput {
                conversation: CONVERSATION,
                call_id: "exec-1".to_string(),
                chunk: "hello\n".to_string(),
            }
        );
    }

    #[test]
    fn 適用済みのファイル変更を翻訳する() {
        let event = translate(
            "item/completed",
            r#"{"item":{"type":"fileChange","id":"exec-817a10b8","changes":[{"path":"/tmp/nebula-codex-test/note.txt","kind":{"type":"add"},"diff":"ok\n"}],"status":"completed"},"threadId":"019fedc4","turnId":"t","completedAtMs":1786400486845}"#,
        );
        assert_eq!(
            event,
            CodexEvent::PatchApplied {
                conversation: CONVERSATION,
                files: vec![PathBuf::from("/tmp/nebula-codex-test/note.txt")],
            }
        );
    }

    #[test]
    fn 適用前のファイル変更は生イベントで通す() {
        let event = translate(
            "item/completed",
            r#"{"item":{"type":"fileChange","id":"exec-817a10b8","changes":[{"path":"/tmp/note.txt","kind":{"type":"add"},"diff":"ok\n"}],"status":"inProgress"},"threadId":"019fedc4","turnId":"t"}"#,
        );
        assert!(matches!(event, CodexEvent::Raw { .. }));
    }

    #[test]
    fn ターン完了にトークン使用量を載せる() {
        let usage = CodexTokenUsage {
            input_tokens: 57797,
            cached_input_tokens: 52224,
            output_tokens: 204,
            total_tokens: 58001,
        };
        let event = translate_notification(
            "turn/completed",
            &params(
                r#"{"threadId":"019fedc4","turn":{"id":"019fedc4-0d79","items":[],"itemsView":"summary","status":"completed","error":null,"durationMs":14667}}"#,
            ),
            Some(CONVERSATION),
            Some(usage),
        );
        assert_eq!(
            event,
            CodexEvent::TurnComplete {
                conversation: CONVERSATION,
                token_usage: Some(usage),
            }
        );
    }

    #[test]
    fn エラー通知を翻訳する() {
        let event = translate(
            "error",
            r#"{"threadId":"019fedc4","turnId":"t","willRetry":false,"error":{"message":"usage limit exceeded","codexErrorInfo":"usageLimitExceeded","additionalDetails":null}}"#,
        );
        assert_eq!(
            event,
            CodexEvent::Error {
                conversation: CONVERSATION,
                message: "usage limit exceeded".to_string(),
            }
        );
    }

    #[test]
    fn 未知の通知は生イベントで通す() {
        let event = translate(
            "turn/diff/updated",
            r#"{"threadId":"019fedc4","turnId":"t","diff":"diff --git a/note.txt b/note.txt\n"}"#,
        );
        let CodexEvent::Raw {
            conversation,
            method,
            payload,
        } = event
        else {
            panic!("生イベントになりませんでした");
        };
        assert_eq!(conversation, Some(CONVERSATION));
        assert_eq!(method, "turn/diff/updated");
        assert!(payload.contains("diff --git"));
    }

    #[test]
    fn 会話を解決できない通知は会話なしの生イベントになる() {
        let event = translate_notification(
            "item/agentMessage/delta",
            &params(r#"{"threadId":"未登録","itemId":"m","turnId":"t","delta":"x"}"#),
            None,
            None,
        );
        assert!(matches!(
            event,
            CodexEvent::Raw {
                conversation: None,
                ..
            }
        ));
    }

    #[test]
    fn トークン使用量は累計を採る() {
        let usage = parse_token_usage(&params(
            r#"{"threadId":"019fedc4","turnId":"t","tokenUsage":{"total":{"totalTokens":58001,"inputTokens":57797,"cachedInputTokens":52224,"cacheWriteInputTokens":0,"outputTokens":204,"reasoningOutputTokens":7},"last":{"totalTokens":14589,"inputTokens":14584,"cachedInputTokens":14080,"cacheWriteInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0},"modelContextWindow":258400}}"#,
        ))
        .expect("使用量の解析");
        assert_eq!(usage.total_tokens, 58001);
        assert_eq!(usage.input_tokens, 57797);
        assert_eq!(usage.cached_input_tokens, 52224);
        assert_eq!(usage.output_tokens, 204);
    }

    #[test]
    fn コマンド承認要求を翻訳する() {
        let request = translate_approval(
            EXEC_APPROVAL,
            &params(
                r#"{"threadId":"019fedc4","turnId":"t","itemId":"exec-1","startedAtMs":1,"command":"rm -rf build","cwd":"/tmp","reason":null,"approvalId":null}"#,
            ),
            "42",
            None,
        )
        .expect("承認要求の翻訳");
        assert_eq!(request.request_id, "42");
        assert_eq!(request.kind, CodexApprovalKind::ExecCommand);
        assert_eq!(request.detail, "rm -rf build");
        assert_eq!(request.summary, "コマンドの実行を許可しますか");
    }

    #[test]
    fn ファイル変更の承認要求には控えた差分を添える() {
        let (item_id, diff) = patch_preview(&params(
            r#"{"type":"fileChange","id":"exec-817a","changes":[{"path":"/tmp/note.txt","kind":{"type":"add"},"diff":"+ok\n"}],"status":"inProgress"}"#,
        ))
        .expect("差分の抽出");
        assert_eq!(item_id, "exec-817a");

        let request = translate_approval(
            PATCH_APPROVAL,
            &params(
                r#"{"threadId":"019fedc4","turnId":"t","itemId":"exec-817a","startedAtMs":1,"reason":"追加の書き込み権限が必要です","grantRoot":null}"#,
            ),
            "req-9",
            Some(diff),
        )
        .expect("承認要求の翻訳");
        assert_eq!(request.kind, CodexApprovalKind::ApplyPatch);
        assert_eq!(request.summary, "追加の書き込み権限が必要です");
        assert!(request.detail.contains("/tmp/note.txt"));
        assert!(request.detail.contains("+ok"));
    }

    #[test]
    fn 承認以外のサーバー要求は翻訳しない() {
        assert!(translate_approval("attestation/generate", &json!({}), "1", None).is_none());
    }

    #[test]
    fn 判断はapp_serverの語彙に変換される() {
        assert_eq!(
            decision_result(CodexApprovalDecision::Approve)["decision"],
            "accept"
        );
        assert_eq!(
            decision_result(CodexApprovalDecision::ApproveForSession)["decision"],
            "acceptForSession"
        );
        assert_eq!(
            decision_result(CodexApprovalDecision::Deny)["decision"],
            "decline"
        );
        assert_eq!(
            decision_result(CodexApprovalDecision::Abort)["decision"],
            "cancel"
        );
    }

    #[test]
    fn 会話開始のパラメータに実行モードが反映される() {
        let spec = CodexSessionSpec {
            workspace: nebula_protocol::WorkspaceId(1),
            model: Some("gpt-5.6-sol".to_string()),
            approval_policy: CodexApprovalPolicy::OnFailure,
            sandbox_policy: CodexSandboxPolicy::WorkspaceWrite,
            cwd: Some(PathBuf::from("/tmp/work")),
        };
        let params = thread_start_params(&spec);
        assert_eq!(params["cwd"], "/tmp/work");
        assert_eq!(params["model"], "gpt-5.6-sol");
        // OnFailure に対応する値が app-server に無いので on-request に寄せる。
        assert_eq!(params["approvalPolicy"], "on-request");
        assert_eq!(params["sandbox"], "workspace-write");
    }

    #[test]
    fn 添付は画像とそれ以外で項目が変わる() {
        let image = attachment_input(std::path::Path::new("/tmp/shot.PNG"));
        assert_eq!(image["type"], "localImage");
        assert_eq!(image["path"], "/tmp/shot.PNG");

        let file = attachment_input(std::path::Path::new("/tmp/src/main.rs"));
        assert_eq!(file["type"], "mention");
        assert_eq!(file["name"], "main.rs");
        assert_eq!(file["path"], "/tmp/src/main.rs");
    }
}
