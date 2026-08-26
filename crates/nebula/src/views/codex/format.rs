//! 表示用の整形。
//!
//! Markdown の最小限の分解と、ラベル・数値の文字列化だけを集めてある。副作用のない
//! 純粋関数なので、[`chat`](super::chat) 同様 GPUI を起動せずに単体テストできる。

use gpui::Hsla;
use nebula_protocol::{
    CodexApprovalDecision, CodexApprovalKind, CodexApprovalPolicy, CodexSandboxPolicy,
    CodexTokenUsage,
};

// ---------------------------------------------------------------------------
// Markdown の最小限の分解
// ---------------------------------------------------------------------------

/// 応答本文の断片。Markdown のうちコードブロックだけを区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MarkdownSegment {
    Text(String),
    Code {
        language: Option<String>,
        body: String,
    },
}

/// 本文を「通常の文」と「コードブロック」の列に分ける。
///
/// 閉じていないフェンスは末尾までをコードとして扱う。応答をストリーミングしている
/// 途中は必ずこの形になるので、捨てると書きかけのコードが画面から消える。
pub(super) fn split_code_blocks(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut text_lines: Vec<&str> = Vec::new();
    let mut code: Option<(Option<String>, Vec<&str>)> = None;

    for line in text.split('\n') {
        let trimmed = line.trim();
        if let Some((_, body)) = code.as_mut() {
            if trimmed == "```" {
                let (language, body) = code.take().expect("直前に存在を確認している");
                segments.push(MarkdownSegment::Code {
                    language,
                    body: body.join("\n"),
                });
            } else {
                body.push(line);
            }
        } else if let Some(rest) = trimmed.strip_prefix("```") {
            flush_text(&mut segments, &mut text_lines);
            let language = (!rest.trim().is_empty()).then(|| rest.trim().to_string());
            code = Some((language, Vec::new()));
        } else {
            text_lines.push(line);
        }
    }

    match code {
        Some((language, body)) => segments.push(MarkdownSegment::Code {
            language,
            body: body.join("\n"),
        }),
        None => flush_text(&mut segments, &mut text_lines),
    }
    segments
}

fn flush_text(segments: &mut Vec<MarkdownSegment>, lines: &mut Vec<&str>) {
    let joined = lines.join("\n");
    lines.clear();
    let trimmed = joined.trim_matches('\n');
    if !trimmed.trim().is_empty() {
        segments.push(MarkdownSegment::Text(trimmed.to_string()));
    }
}

// ---------------------------------------------------------------------------
// 表示用の整形
// ---------------------------------------------------------------------------

/// 実行コマンドを 1 行にする。空白を含む引数は引用符でくくる。
pub(super) fn format_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// ヘッダに出すトークン使用量。桁が大きいので 1000 単位で丸める。
pub(super) fn format_token_usage(usage: &CodexTokenUsage) -> String {
    format!(
        "↑{} ↓{} 計{}",
        compact_count(usage.input_tokens),
        compact_count(usage.output_tokens),
        compact_count(usage.total_tokens)
    )
}

fn compact_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

pub(super) fn approval_policy_label(policy: CodexApprovalPolicy) -> &'static str {
    match policy {
        CodexApprovalPolicy::Untrusted => "未信頼のみ確認",
        CodexApprovalPolicy::OnFailure => "失敗時に確認",
        CodexApprovalPolicy::OnRequest => "要求時に確認",
        CodexApprovalPolicy::Never => "確認しない",
    }
}

pub(super) fn sandbox_policy_label(policy: CodexSandboxPolicy) -> &'static str {
    match policy {
        CodexSandboxPolicy::ReadOnly => "読み取りのみ",
        CodexSandboxPolicy::WorkspaceWrite => "ワークスペース書込可",
        CodexSandboxPolicy::DangerFullAccess => "制限なし (危険)",
    }
}

pub(super) fn decision_label(decision: CodexApprovalDecision) -> &'static str {
    match decision {
        CodexApprovalDecision::Approve => "許可しました",
        CodexApprovalDecision::ApproveForSession => "このセッションは常に許可します",
        CodexApprovalDecision::Deny => "拒否しました",
        CodexApprovalDecision::Abort => "中止しました",
    }
}

pub(super) fn approval_kind_label(kind: CodexApprovalKind) -> &'static str {
    match kind {
        CodexApprovalKind::ExecCommand => "コマンド実行の承認",
        CodexApprovalKind::ApplyPatch => "ファイル変更の承認",
        CodexApprovalKind::Other => "承認",
    }
}

/// diff 1 行の色。追加は緑、削除は赤、ヘッダは薄く。
pub(super) fn diff_line_color(line: &str, theme: &crate::theme::Theme) -> Hsla {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        theme.text_faint
    } else if line.starts_with('+') {
        theme.git_added
    } else if line.starts_with('-') {
        theme.git_deleted
    } else {
        theme.text_muted
    }
}

pub(super) fn decision_index(decision: CodexApprovalDecision) -> usize {
    match decision {
        CodexApprovalDecision::Approve => 0,
        CodexApprovalDecision::ApproveForSession => 1,
        CodexApprovalDecision::Deny => 2,
        CodexApprovalDecision::Abort => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn コードブロックを本文から切り分ける() {
        let segments = split_code_blocks("説明です\n```rust\nfn main() {}\n```\nおわり");
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Text("説明です".into()),
                MarkdownSegment::Code {
                    language: Some("rust".into()),
                    body: "fn main() {}".into(),
                },
                MarkdownSegment::Text("おわり".into()),
            ]
        );
    }

    #[test]
    fn 言語指定のないコードブロック() {
        let segments = split_code_blocks("```\nls -la\n```");
        assert_eq!(
            segments,
            vec![MarkdownSegment::Code {
                language: None,
                body: "ls -la".into(),
            }]
        );
    }

    #[test]
    fn 閉じていないフェンスは末尾までコードとして扱う() {
        // ストリーミング途中の応答。捨てると書きかけのコードが画面から消える。
        let segments = split_code_blocks("途中です\n```py\nprint(1)");
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Text("途中です".into()),
                MarkdownSegment::Code {
                    language: Some("py".into()),
                    body: "print(1)".into(),
                },
            ]
        );
    }

    #[test]
    fn コードブロックが無ければ本文だけ返す() {
        assert_eq!(
            split_code_blocks("ただの文\n2 行目"),
            vec![MarkdownSegment::Text("ただの文\n2 行目".into())]
        );
    }

    #[test]
    fn 空文字列は断片を生まない() {
        assert!(split_code_blocks("").is_empty());
        assert!(split_code_blocks("\n\n").is_empty());
    }

    #[test]
    fn コードブロック内の空行と字下げを保つ() {
        let segments = split_code_blocks("```\na\n\n    b\n```");
        assert_eq!(
            segments,
            vec![MarkdownSegment::Code {
                language: None,
                body: "a\n\n    b".into(),
            }]
        );
    }

    #[test]
    fn コマンドは空白を含む引数を引用する() {
        assert_eq!(
            format_command(&[
                "git".into(),
                "commit".into(),
                "-m".into(),
                "初回 コミット".into()
            ]),
            "git commit -m \"初回 コミット\""
        );
    }

    #[test]
    fn トークン使用量を短く表す() {
        let usage = CodexTokenUsage {
            input_tokens: 1500,
            cached_input_tokens: 0,
            output_tokens: 250,
            total_tokens: 1_750_000,
        };
        assert_eq!(format_token_usage(&usage), "↑1.5k ↓250 計1.8M");
    }

    #[test]
    fn diff_の追加行と削除行で色が変わる() {
        let theme = crate::theme::Theme::cyber_cosmic();
        assert_eq!(diff_line_color("+ added", &theme), theme.git_added);
        assert_eq!(diff_line_color("- removed", &theme), theme.git_deleted);
        assert_eq!(diff_line_color("@@ -1 +1 @@", &theme), theme.text_faint);
        assert_eq!(diff_line_color(" context", &theme), theme.text_muted);
    }
}
