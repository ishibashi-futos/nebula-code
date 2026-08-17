//! Nebula Code の起動口。
//!
//! 冷間起動 200ms 以下という目標のため、`main` から最初のフレームまでの間には
//! 「テーマとキーバインドの登録」「ウィンドウ生成」しか置かない。
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
    let started = Instant::now();
    let _ = STARTED.set(started);
    let initial_folder = parse_folder_arg();

    Application::new()
        .with_assets(NebulaAssets)
        .run(move |cx: &mut App| {
            theme::init(cx);
            actions::init(cx);

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

/// 引数で渡されたフォルダ。無ければカレントディレクトリを開かない (空の状態で起動する)。
fn parse_folder_arg() -> Option<PathBuf> {
    std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}
