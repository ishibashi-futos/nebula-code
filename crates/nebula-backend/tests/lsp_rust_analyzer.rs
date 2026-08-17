//! 実物の rust-analyzer を起動して LSP 経路が通ることを確かめる結合テスト。
//!
//! このリポジトリ自身をワークスペースにして `crates/nebula-core/src/buffer.rs` の
//! `Rope` にホバーする。rust-analyzer は初回の索引付けに数十秒かかり、その間は
//! ホバーが `null` で返るため、内容が返るまで再試行する。
//!
//! 通常の `cargo test` を遅くしないよう `#[ignore]` にしてある。実行するには:
//!
//! ```text
//! cargo test -p nebula-backend --test lsp_rust_analyzer -- --ignored --nocapture
//! ```

use nebula_backend::lsp::LspService;
use nebula_protocol::{DetectedTools, Event, LspServerState, Position};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 索引付けを待つ上限。手元の実測では 30 秒前後で最初のホバーが返る。
const INDEXING_BUDGET: Duration = Duration::from_secs(180);

fn repo_root() -> PathBuf {
    // `crates/nebula-backend` から 2 つ上がリポジトリのルート。
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("リポジトリのルート")
        .to_path_buf()
}

/// `needle` が最初に現れる位置を行・char 桁で返す。
fn locate(text: &str, needle: &str) -> Position {
    for (row, line) in text.split('\n').enumerate() {
        if let Some(byte) = line.find(needle) {
            let column = line[..byte].chars().count() as u32;
            return Position::new(row as u32, column);
        }
    }
    panic!("{needle} が見つかりません");
}

#[tokio::test]
#[ignore = "rust-analyzer の索引付けに時間がかかるため手動実行用"]
async fn rust_analyzer_のホバーが返る() {
    let root = repo_root();
    let path = root.join("crates/nebula-core/src/buffer.rs");
    let text = std::fs::read_to_string(&path).expect("buffer.rs を読む");
    let position = locate(&text, "Rope::from_str");

    let (events, mut received) = broadcast::channel(4096);
    let service = LspService::new(events, DetectedTools::default());

    service
        .ensure_server(&root, "rust")
        .await
        .expect("rust-analyzer の起動");
    let statuses = service.statuses();
    println!("状態: {statuses:?}");
    assert_eq!(
        statuses[0].state,
        LspServerState::Running,
        "rust-analyzer が起動していない"
    );

    service.did_open(&path, "rust", 1, &text).await;

    let started = Instant::now();
    let hover = loop {
        match service.hover(&path, &text, position).await {
            Ok(Some(hover)) if !hover.contents.trim().is_empty() => break hover,
            other => {
                assert!(
                    started.elapsed() < INDEXING_BUDGET,
                    "{:?} 以内にホバーが返らなかった (直近の結果: {other:?})",
                    INDEXING_BUDGET
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };

    println!("{:?} でホバーが返った:\n{}", started.elapsed(), hover.contents);
    // ropey の Rope 型に当たっていれば型名が本文に出る。
    assert!(
        hover.contents.contains("Rope"),
        "ホバー内容が想定と違う: {}",
        hover.contents
    );

    // 索引付けが済んだ後なら他の要求もすぐ返る。まとめて経路を確認する。
    let symbols = service.document_symbols(&path).await.expect("シンボル一覧");
    println!("シンボル: {:?}", symbols.iter().map(|s| &s.name).collect::<Vec<_>>());
    assert!(
        symbols.iter().any(|symbol| symbol.name == "TextBuffer"),
        "TextBuffer が一覧にない"
    );

    let definitions = service
        .definition(&path, &text, position)
        .await
        .expect("定義ジャンプ");
    println!("定義: {definitions:?}");
    assert!(
        definitions
            .iter()
            .any(|link| link.path.to_string_lossy().contains("ropey")),
        "Rope の定義が ropey のソースを指していない"
    );

    // `Rope::` の直後で補完すると関連関数が並ぶ。
    let after_colons = Position::new(position.row, position.column + 6);
    let completions = service
        .completion(&path, &text, after_colons, Some(":"))
        .await
        .expect("補完");
    println!("補完候補数: {}", completions.len());
    assert!(
        completions.iter().any(|item| item.label == "from_str"),
        "from_str が補完候補にない"
    );

    // 進捗と変更通知の扱いを確認する。フル同期を受け付けないサーバーだと
    // 起動時に警告を流す実装なので、それが出ていないこと = 同期できている。
    while let Ok(event) = received.try_recv() {
        if let Event::LspMessage { message, .. } = event {
            assert!(
                !message.contains("文書変更の通知を受け付けません"),
                "rust-analyzer がフルテキスト同期を受け付けていない"
            );
        }
    }

    service.shutdown().await;
}
