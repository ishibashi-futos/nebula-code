//! 古いバックエンドプロセスを終わらせるための Windows 固有の手当て。
//!
//! 接続そのものは `nebula_protocol::transport` が担う。ここに残るのは
//! 「プロセスをどう終わらせ、どう終了を見届けるか」だけ。

use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
// SYNCHRONIZE は任意のカーネルオブジェクトに使える標準アクセス権だが、
// windows-sys はファイル用の一覧の中にしか出力していない。値は同じなので
// プロセスハンドルにもそのまま使う。
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

/// 応答しないバックエンドを終わらせる。Unix 側の SIGTERM に相当する。
///
/// `TerminateProcess` に SIGTERM のような「穏やかに終わる」余地は無いが、
/// これを呼ぶのは既に `Request::Shutdown` に応答しないと分かった相手だけなので
/// 実質的な差は無い。
pub fn terminate(pid: u32) {
    // SAFETY: pid はハンドシェイクで得た実在のプロセス ID。開けたハンドルは
    // この関数を抜ける前に必ず閉じる。
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

/// バックエンドが終わり切るのを待つ。
///
/// Unix 側はソケットファイルが消えるのを合図にしているが、パイプ名は
/// ハンドルが 1 つでも生きていれば引けてしまう。こちらがまだ握っている接続も
/// そのハンドルに数えられるので、名前の有無では終了を判定できない。
/// ハンドシェイクで得た pid のプロセスそのものが終わるのを待つ。
pub fn wait_for_backend_gone(_endpoint: &Path, pid: u32, timeout: Duration) -> bool {
    // SAFETY: pid はハンドシェイクで得た実在のプロセス ID。開けたハンドルは
    // この関数を抜ける前に必ず閉じる。
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // 開けないのは既に終わっているか、権限が無いか。どちらにせよ
            // 待っても状況は変わらないので、終わったものとして先へ進む。
            return true;
        }
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        let waited = WaitForSingleObject(handle, millis);
        CloseHandle(handle);
        waited == WAIT_OBJECT_0
    }
}
