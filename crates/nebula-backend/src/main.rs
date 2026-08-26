//! バックエンドプロセスの起動口。
//!
//! 通常は GUI から自動起動されるが、`--endpoint` を指定して単体でも動かせる。

use nebula_backend::{BackendState, git_watch, ipc, tools};
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    reset_signal_state();
    let endpoint = parse_endpoint_arg().unwrap_or_else(nebula_protocol::default_endpoint);

    // ワーカースレッド数を絞る。バックエンドの仕事は I/O 待ちが主体で、
    // 論理コア数ぶんのスレッドを立てても起動コストが増えるだけ。
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("nebula-backend: ランタイムを初期化できません: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        // 待ち受けは検出の完了を待たずに開く。検出に時間がかかるツールが 1 つでも
        // あると、その間 GUI が「接続できませんでした」になってしまうため。
        // 検出は切り離して裏で走らせ、終わり次第 Event::ToolsDetected で届ける。
        let state = BackendState::new(nebula_protocol::DetectedTools::default());
        let detect_state = state.clone();
        tokio::spawn(async move {
            let detected = tools::detect_all().await;
            detect_state.apply_detected_tools(detected);
        });
        // 保存や外部での git 操作のような、バックエンドを経由しないファイル変更でも
        // git status を追従させる (詳細は git_watch のモジュール doc を参照)。
        git_watch::spawn(state.clone());
        match ipc::serve(&endpoint, state).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("nebula-backend: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    })
}

/// GUI から受け継いだシグナルの状態を既定へ戻す。
///
/// シグナルマスクと「無視 (SIG_IGN)」の設定は `exec` を越えて子へ引き継がれる。
/// GUI プロセスはこれらを止めた状態でバックエンドを起動するため、そのままだと
/// **SIGCHLD が届かない**。tokio が終了した子プロセスを回収できず、外部ツールの
/// 検出 (`tools::detect_all`) が永久に終わらないので、待ち受けを開く前に停止する。
/// GUI 側からは「バックエンドに接続できませんでした」に見える。
/// 同時に SIGTERM も届かなくなり、`pkill` で止められないプロセスが残る。
#[cfg(unix)]
fn reset_signal_state() {
    // SAFETY: 他のスレッドを作る前の main 先頭でのみ呼ぶ。ここで設定した
    // マスクは以降に作られる全スレッドへ引き継がれる。
    unsafe {
        let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        if libc::sigemptyset(empty.as_mut_ptr()) == 0 {
            libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
        }
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}

/// Windows には `exec` を越えて引き継がれるシグナルマスクという仕組みが無く、
/// 子プロセスの回収も `Child::wait` (`WaitForSingleObject`) で完結するため、
/// SIGCHLD が届かないことによる取りこぼしも起こらない。戻すべき状態が無い。
#[cfg(windows)]
fn reset_signal_state() {}

fn parse_endpoint_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--endpoint" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}
