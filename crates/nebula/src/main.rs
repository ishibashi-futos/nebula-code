//! Nebula Code の起動口。
//!
//! 冷間起動 200ms 以下という目標のため、`main` から最初のフレームまでの間には
//! 「テーマとキーバインドの登録」「フォントの登録」「ウィンドウ生成」しか置かない。
//! バックエンド接続・言語文法のコンパイル・git 状態の取得はすべてフレーム後に回す。
//!
//! `nebula update` (自己更新) はこの計測対象に含めない。ウィンドウを一切開かず、
//! `Application::new().run()` に入る前に分岐して結果と終了コードだけを返す。

mod actions;
mod app;
mod assets;
mod ipc_client;
mod session;
mod theme;
mod ui;
mod update;
mod views;

use app::NebulaApp;
use assets::NebulaAssets;
use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use std::ffi::OsString;
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
    // `nebula update` も curl を子プロセスとして起動し、その完了を待つ
    // (`Command::output`/`status` は内部で `wait()` する)。GUI 起動時と同じ理由
    // でここを一番先に通す必要があるため、引数の分岐より前に置く
    // (詳細は reset_signal_state のドキュメントを参照)。
    reset_signal_state();

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let initial_folder = match update::parse_cli(&args) {
        Ok(update::Cli::Update(update_args)) => {
            // 自己更新はウィンドウを一切開かない。CLI として結果を出して
            // 終了コードを返すだけの経路なので、gpui に入る前にここで完結させる。
            std::process::exit(update::run_update(update_args));
        }
        Ok(update::Cli::OpenEditor(folder)) => folder.filter(|p| p.is_dir()),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(update::EXIT_ERROR);
        }
    };

    let started = Instant::now();
    let _ = STARTED.set(started);

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

/// 子プロセス (バックエンド・`nebula update` の curl) を確実に回収できるよう、
/// シグナルの状態を既定へ戻す。
///
/// シグナルマスクと「無視 (SIG_IGN)」設定は `exec` を越えて子へ引き継がれる。この
/// プロセス自身がどんな状態で起動されたかに関わらず、子プロセスを spawn する
/// 前に一度戻しておけば、子の回収 (`Child::wait()` や `Command::output()`/`status()`
/// が内部で行う待ち合わせ) に影響されない。バックエンド側にも同名の関数
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
