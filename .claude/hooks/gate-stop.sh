#!/usr/bin/env bash
# Claude Code の Stop フックから呼ばれる、作業完了時の整形・Lintゲート。
#
# gate-commit.sh はコミットを試みたときしか発動しない。commit せずに
# 作業を終える回 (このリポジトリでは珍しくない: 「作業ツリーに変更を
# 残すだけでコミットしない」指示のタスクがある) を素通りさせないための
# 保険として、作業完了そのものにもゲートをかける。
#
# gate-commit.sh を通ってコミット済みなら、コミット時点のツリーは
# 既に緑になっているはずなので、ここで再度赤が出るのは「コミット後に
# さらに手を加えた」場合だけ。コミット済みの内容を直すための
# 二重コミットにはならない。

set -uo pipefail
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$dir/lib-fmt-clippy.sh"

run_fmt_clippy_check
if [ "$FMT_STATUS" -eq 0 ] && [ "$CLIPPY_STATUS" -eq 0 ]; then
  exit 0
fi

reason="$(build_fmt_clippy_reason "整形/Lintゲートに引っかかりました。直してから完了してください。")"

jq -n --arg reason "$reason" '{decision: "block", reason: $reason}'
