//! ファイル変更をきっかけにした git status の再取得。
//!
//! `dispatch::emit_git_status` は Stage/Unstage/Commit/Checkout/Push/Pull など
//! GUI からの明示的な git 操作のあとにしか呼ばれない。外部エディタでの保存や、
//! 別ターミナルでの `git commit`/`git checkout` のように **バックエンドを経由しない**
//! 変更は `watch.rs` の `Event::FilesChanged` としてしか観測できず、そのままでは
//! エクスプローラーや Git パネルの表示が古いまま取り残される。ここでそのギャップを埋める。
//!
//! `FilesChanged` 自体は `watch.rs` 内で既に 150ms 単位にまとめられているが、
//! `cargo build` のような書き込み嵐が続く間は次々届く。そのたびに `git status`
//! (外部プロセス起動) を叩くと無駄が大きいので、ここでさらに [`QUIET_WINDOW`] だけ
//! 静かになるのを待ってから 1 回だけ取り直す。待ち自体は `tokio::time::sleep` に
//! 任せ、「待っている間に新しいイベントが来ていないか」の判定だけを
//! [`GitStatusDebounce`] という純粋な部品に切り出してあるので、実時間を進めずに
//! テストできる。

use crate::dispatch::emit_git_status;
use crate::state::BackendState;
use nebula_protocol::{Event, WorkspaceId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 最後の `FilesChanged` からこの時間だけ静かなら git status を取り直す。
/// ビルド出力の書き込み嵐を 1 回に畳み込むのに十分で、かつ体感の遅れとしても
/// 許容できる範囲 (300〜500ms) に収める。
const QUIET_WINDOW: Duration = Duration::from_millis(400);

/// バックグラウンドで `Event::FilesChanged` の待ち受けを始める。
///
/// `ipc::serve` が `state` を消費する前、`main.rs` から 1 度だけ呼ぶ。戻り値は無く、
/// 生成したタスクはプロセスが終わるまで (= `state.events` の送信側が消えて
/// `recv` が `Closed` を返すまで) 動き続ける。
pub fn spawn(state: Arc<BackendState>) {
    let mut events = state.events.subscribe();
    let debounce = Arc::new(Mutex::new(GitStatusDebounce::default()));
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                // 取りこぼしても実害はない。次の FilesChanged がまた同じ
                // ワークスペースについて届くので、そこで改めて待てばよい。
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            let Event::FilesChanged { workspace, .. } = event else {
                continue;
            };
            let scheduled_at = debounce
                .lock()
                .expect("デバウンス表のロック")
                .touch(workspace, Instant::now());

            let state = state.clone();
            let debounce = debounce.clone();
            tokio::spawn(async move {
                tokio::time::sleep(QUIET_WINDOW).await;
                let fire = debounce
                    .lock()
                    .expect("デバウンス表のロック")
                    .due(workspace, scheduled_at);
                if fire {
                    emit_git_status(&state, workspace).await;
                }
            });
        }
    });
}

/// 「静穏期間が過ぎるまで発火を待つ」判定だけを切り出した純粋な部品。
///
/// 実際の待ち時間は呼び出し側 (`tokio::time::sleep`) が持つ。ここではワークスペースごとに
/// 直近のイベント時刻だけを覚えておき、待機タスクが起きたときに「自分が記録した
/// 時刻のまま (＝もっと新しい `touch` に上書きされていない)」かどうかだけを見る。
/// 上書きされていれば、その新しい `touch` を起点にした待機タスクが後で同じ判定をするので
/// 何もしなくてよい (二重発火の防止)。
#[derive(Default)]
struct GitStatusDebounce {
    /// ワークスペースごとの直近のイベント時刻。
    last_event: HashMap<WorkspaceId, Instant>,
}

impl GitStatusDebounce {
    /// イベントを記録し、待機タスクが後で照合するための基準時刻を返す。
    fn touch(&mut self, workspace: WorkspaceId, now: Instant) -> Instant {
        self.last_event.insert(workspace, now);
        now
    }

    /// `scheduled_at` を基準にした待機が、いま発火してよいか。
    /// 発火する場合は記録を消す (同じ静穏期間で二重に発火しないため)。
    fn due(&mut self, workspace: WorkspaceId, scheduled_at: Instant) -> bool {
        if self.last_event.get(&workspace) == Some(&scheduled_at) {
            self.last_event.remove(&workspace);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 静穏期間は指定範囲の300から500ミリ秒に収まる() {
        assert!(QUIET_WINDOW >= Duration::from_millis(300));
        assert!(QUIET_WINDOW <= Duration::from_millis(500));
    }

    #[test]
    fn 単発のイベントは静穏期間の終わりに発火する() {
        let mut debounce = GitStatusDebounce::default();
        let ws = WorkspaceId(1);
        let scheduled = debounce.touch(ws, Instant::now());
        assert!(debounce.due(ws, scheduled));
    }

    #[test]
    fn 静穏期間中に来た新しいイベントは古い待機を無効化する() {
        let mut debounce = GitStatusDebounce::default();
        let ws = WorkspaceId(1);
        let t0 = Instant::now();
        let first = debounce.touch(ws, t0);
        let second = debounce.touch(ws, t0 + Duration::from_millis(50));
        assert!(!debounce.due(ws, first), "追い越された古い待機は発火しない");
        assert!(debounce.due(ws, second), "最後の待機だけが発火する");
    }

    #[test]
    fn 一度発火したら同じ待機で再び発火しない() {
        let mut debounce = GitStatusDebounce::default();
        let ws = WorkspaceId(1);
        let scheduled = debounce.touch(ws, Instant::now());
        assert!(debounce.due(ws, scheduled));
        assert!(
            !debounce.due(ws, scheduled),
            "発火時に記録が消えるので二度目は起きない"
        );
    }

    #[test]
    fn ワークスペースが異なれば互いの待機に影響しない() {
        let mut debounce = GitStatusDebounce::default();
        let a = WorkspaceId(1);
        let b = WorkspaceId(2);
        let t0 = Instant::now();
        let scheduled_a = debounce.touch(a, t0);
        debounce.touch(b, t0 + Duration::from_millis(10));
        assert!(
            debounce.due(a, scheduled_a),
            "別ワークスペースの更新に巻き込まれない"
        );
    }

    #[test]
    fn 記録の無いワークスペースは発火しない() {
        let mut debounce = GitStatusDebounce::default();
        assert!(!debounce.due(WorkspaceId(1), Instant::now()));
    }
}
