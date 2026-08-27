#!/usr/bin/env bash
# Claude Code の PreToolUse フック (Bash) から呼ばれる、コミット前の整形・Lintゲート。
#
# 整形/Lintが割れたコードがコミットされるのを防ぐ。`git commit` を含む Bash
# コマンドだけを検査対象にし、それ以外 (git status, git add など) は
# 何もせず即座に許可する。
#
# 既知の限界:
# - 検査するのは作業ツリー。`cargo fmt` で直してもステージ済みの中身までは
#   直らないので、直した後は該当ファイルを git add し直す必要がある
#   (reason にその指示を含めている)。
# - Claude Code 経由の commit しか止められない。人間が直接叩く commit や
#   他セッションの commit を止めたいなら .git/hooks/pre-commit を使うこと。
# - この検査だけでは「一度も commit しないまま作業を終える」場合を
#   捕まえられない。そちらは gate-stop.sh (Stop フック) の役目。

set -uo pipefail
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$dir/lib-fmt-clippy.sh"

input="$(cat)"
command="$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null)"

if [ -z "$command" ]; then
  exit 0
fi
if ! printf '%s' "$command" | grep -qE '\bgit\b.*\bcommit\b'; then
  exit 0
fi

run_fmt_clippy_check
if [ "$FMT_STATUS" -eq 0 ] && [ "$CLIPPY_STATUS" -eq 0 ]; then
  exit 0
fi

reason="$(build_fmt_clippy_reason "コミット前の整形/Lintゲートに引っかかりました。直してからコミットし直してください。
(cargo fmt で直した場合、ステージ済みの内容は変わらないので該当ファイルを git add し直すこと)")"

jq -n --arg reason "$reason" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: $reason
  }
}'
