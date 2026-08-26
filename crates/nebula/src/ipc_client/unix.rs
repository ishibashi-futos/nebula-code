//! 古いバックエンドプロセスを終わらせるための Unix 固有の手当て。
//!
//! 接続そのものは `nebula_protocol::transport` が担う。ここに残るのは
//! 「プロセスをどう終わらせ、どう終了を見届けるか」だけ。

use std::path::Path;
use std::time::{Duration, Instant};

/// 応答しないバックエンドを終わらせる。
///
/// SIGTERM は対象プロセスに既定の終了処理を促すだけで、こちらのメモリには
/// 一切触れない。
pub fn terminate(pid: u32) {
    // SAFETY: pid はハンドシェイクで得た実在のプロセス ID。
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// バックエンドが終わり切るのを待つ。
///
/// バックエンドは `Request::Shutdown` を受けて後始末を終えると、待ち受けていた
/// ソケットファイルを削除する。それを「終了し切った」の合図として使う。
/// pid は見ない。プロセスの消滅だけでは後始末まで終わったかが分からないのに対し、
/// ソケットファイルの消滅は後始末の完了そのものを表すため。
pub fn wait_for_backend_gone(endpoint: &Path, _pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !endpoint.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !endpoint.exists()
}
