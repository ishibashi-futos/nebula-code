#!/usr/bin/env bash
# cargo fmt --check / cargo clippy -D warnings を実行する共通処理。
#
# gate-commit.sh (コミット前) と gate-stop.sh (作業完了時) の両方から
# source して使う。チェック本体を1箇所にまとめ、判定タイミングだけを
# 呼び出し側で変える (DRY)。
#
# cwd はプロジェクトルート (またはその配下) である前提。cargo は Cargo.toml を
# 上位ディレクトリへ辿って見つけるので、明示的な cd は不要。

run_fmt_clippy_check() {
  FMT_LOG="$(mktemp)"
  CLIPPY_LOG="$(mktemp)"
  trap 'rm -f "$FMT_LOG" "$CLIPPY_LOG"' EXIT

  cargo fmt --all -- --check >"$FMT_LOG" 2>&1
  FMT_STATUS=$?

  cargo clippy --workspace --all-targets --locked -- -D warnings >"$CLIPPY_LOG" 2>&1
  CLIPPY_STATUS=$?
}

# $1: reason の先頭に置く一言。失敗したコマンドの出力末尾を続けて積む。
build_fmt_clippy_reason() {
  local reason="$1"
  if [ "$FMT_STATUS" -ne 0 ]; then
    reason+=$'\n\n'"### cargo fmt --all -- --check (失敗)"$'\n'
    reason+="$(tail -n 40 "$FMT_LOG")"
  fi
  if [ "$CLIPPY_STATUS" -ne 0 ]; then
    reason+=$'\n\n'"### cargo clippy --workspace --all-targets --locked -- -D warnings (失敗)"$'\n'
    reason+="$(tail -n 80 "$CLIPPY_LOG")"
  fi
  printf '%s' "$reason"
}
