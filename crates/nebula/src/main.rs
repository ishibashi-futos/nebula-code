//! Nebula Code の起動口。
//!
//! 冷間起動 200ms 以下という目標のため、`main` から最初のフレームまでの間には
//! 「テーマとキーバインドの登録」「フォントの登録」「ウィンドウ生成」しか置かない。
//! バックエンド接続・言語文法のコンパイル・git 状態の取得はすべてフレーム後に回す。

mod actions;
mod app;
mod assets;
mod ipc_client;
mod session;
mod theme;
mod ui;
mod views;

use app::NebulaApp;
use assets::NebulaAssets;
use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

/// プロセスが `main` に入った時刻。最初のフレームまでの時間を測るために使う。
///
/// 起動時間の指標は「ウィンドウ生成が返るまで」ではなく
/// 「最初の描画が終わるまで」でなければ意味がない。前者は描画前に返るため。
static STARTED: OnceLock<Instant> = OnceLock::new();

pub fn startup_elapsed_ms() -> Option<f64> {
    STARTED.get().map(|t| t.elapsed().as_secs_f64() * 1000.0)
}

/// 起動計測を有効にするか。環境変数を毎回読むと計測自体に乗るので 1 回だけ判定する。
pub fn trace_startup() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NEBULA_TRACE_STARTUP").is_some())
}

fn main() {
    reset_signal_state();
    let started = Instant::now();
    let _ = STARTED.set(started);
    let initial_folder = parse_folder_arg();

    Application::new()
        .with_assets(NebulaAssets)
        .run(move |cx: &mut App| {
            theme::init(cx);
            actions::init(cx);

            // JetBrains Mono を埋め込みバイト列から登録する。ウィンドウを開く前に
            // 済ませないと初回フレームがフォールバック書体で描かれてしまう。
            // 失敗してもプロセスは止めず、警告のみ出して等幅フォールバックへ委ねる。
            if let Err(err) = cx.text_system().add_fonts(assets::mono_font_bytes()) {
                eprintln!("nebula: JetBrains Mono の登録に失敗しました: {err}");
            }

            let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Nebula Code".into()),
                        // タイトルバーを自前で描く。OS 標準の灰色帯が入るとテーマが分断されるため。
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.), px(9.))),
                    }),
                    ..Default::default()
                },
                |_window, cx| cx.new(|cx| NebulaApp::new(initial_folder, cx)),
            );

            if let Err(e) = window {
                eprintln!("nebula: ウィンドウを作成できません: {e}");
                cx.quit();
                return;
            }
            cx.activate(true);

            // 起動時間は環境変数を付けたときだけ出す。常時出力すると
            // 標準エラーへの書き込みが計測そのものに乗ってしまう。
            // 最初のフレームの時刻は NebulaApp::render が別途出す。
            if trace_startup() {
                eprintln!(
                    "nebula: main からウィンドウ生成まで {:.1}ms",
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
        });
}

/// 子プロセス (バックエンド) を確実に回収できるよう、シグナルの状態を既定へ戻す。
///
/// シグナルマスクと「無視 (SIG_IGN)」設定は `exec` を越えて子へ引き継がれる。この
/// プロセス自身がどんな状態で起動されたかに関わらず、`nebula-backend` を spawn する
/// 前に一度戻しておけば、子の回収 (`Child::wait()`) や将来シグナルハンドラ経由の
/// 回収を足す場合にも影響されない。バックエンド側にも同名の関数
/// (`nebula-backend/src/main.rs`) があるが、GUI は nebula-protocol と
/// nebula-core にしか依存しておらず import できないため、ここに複製している。
fn reset_signal_state() {
    // SAFETY: 他のスレッドを作る前の main 先頭でのみ呼ぶ。ここで設定したマスクは
    // 以降に作られる全スレッドへ引き継がれる。
    unsafe {
        let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        if libc::sigemptyset(empty.as_mut_ptr()) == 0 {
            libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
        }
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}

/// 引数で渡されたフォルダ。無ければカレントディレクトリを開かない (空の状態で起動する)。
fn parse_folder_arg() -> Option<PathBuf> {
    std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}
