//! バックエンドプロセスの起動口。
//!
//! 通常は GUI から自動起動されるが、`--socket` を指定して単体でも動かせる。

use nebula_backend::{BackendState, ipc, tools};
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    let socket_path = parse_socket_arg().unwrap_or_else(nebula_protocol::default_socket_path);

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
        let detected = tools::detect_all().await;
        let state = BackendState::new(detected);
        match ipc::serve(&socket_path, state).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("nebula-backend: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    })
}

fn parse_socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}
