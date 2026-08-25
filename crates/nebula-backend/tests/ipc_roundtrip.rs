//! IPC 経路の結合テスト。
//!
//! バックエンドを実際に立ち上げ、GUI と同じフレーム形式で要求を送って応答を検査する。
//! 各サービスの単体テストでは、プロトコル・フレーミング・ディスパッチを跨いだ
//! 取り違え (要求と応答の型がずれている等) が見つからないため、ここで通しで確認する。

use nebula_backend::{BackendState, ipc, tools};
use nebula_protocol::{
    ClientMessage, Edit, FrameDecoder, ListMarker, PROTOCOL_VERSION, PreviewBlock, Request,
    RequestId, Response, ServerMessage, TextRange, encode_frame,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// テスト用のバックエンドを別スレッドで動かし、接続済みのソケットを返す。
struct Harness {
    stream: UnixStream,
    decoder: FrameDecoder,
    socket: PathBuf,
    workdir: PathBuf,
}

impl Harness {
    fn start(name: &str) -> Self {
        let unique = format!("{}-{}", std::process::id(), name);
        let socket = std::env::temp_dir().join(format!("nebula-it-{unique}.sock"));
        let workdir = std::env::temp_dir().join(format!("nebula-it-{unique}"));
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(socket.with_extension("lock"));
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).expect("作業ディレクトリの作成");

        let socket_for_server = socket.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("ランタイムの構築");
            runtime.block_on(async move {
                let detected = tools::detect_all().await;
                let state = BackendState::new(detected);
                let _ = ipc::serve(&socket_for_server, state).await;
            });
        });

        // 起動を待つ。
        let mut stream = None;
        for _ in 0..200 {
            if let Ok(s) = UnixStream::connect(&socket) {
                stream = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let stream = stream.expect("バックエンドに接続できない");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("読み取りタイムアウトの設定");

        Self {
            stream,
            decoder: FrameDecoder::new(),
            socket,
            workdir,
        }
    }

    /// 要求を送り、対応する応答が来るまで読む。
    ///
    /// 途中で届くイベントは読み飛ばす。イベントは要求と非同期に流れてくるため。
    fn request(&mut self, request: Request) -> Result<Response, String> {
        let id = RequestId::next();
        let frame = encode_frame(&ClientMessage::Request { id, request }).expect("符号化");
        self.stream.write_all(&frame).expect("送信");

        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            while let Ok(Some(message)) = self.decoder.next_message::<ServerMessage>() {
                if let ServerMessage::Response {
                    id: response_id,
                    result,
                } = message
                    && response_id == id
                {
                    return result.map_err(|e| e.to_string());
                }
            }
            let read = self.stream.read(&mut chunk).expect("受信");
            assert!(read > 0, "接続が切れた");
            self.decoder.feed(&chunk[..read]);
        }
    }

    fn write_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.workdir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("親フォルダの作成");
        }
        std::fs::write(&path, contents).expect("テスト用ファイルの書き込み");
        path
    }

    fn root(&self) -> &Path {
        &self.workdir
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.request(Request::Shutdown);
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.socket.with_extension("lock"));
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[test]
fn 握手からバッファ編集保存までを通しで行える() {
    let mut h = Harness::start("full");

    // 握手
    match h.request(Request::Handshake {
        protocol_version: PROTOCOL_VERSION,
    }) {
        Ok(Response::Handshake(info)) => {
            assert_eq!(info.protocol_version, PROTOCOL_VERSION);
            assert!(info.pid > 0);
        }
        other => panic!("握手に失敗: {other:?}"),
    }

    // ワークスペースを開く
    let workspace = match h.request(Request::OpenWorkspace {
        root: h.root().to_path_buf(),
    }) {
        Ok(Response::Workspace(info)) => {
            assert_eq!(info.root, h.root());
            info.id
        }
        other => panic!("ワークスペースを開けない: {other:?}"),
    };

    // ファイルを開く
    let path = h.write_file("main.rs", "fn main() {\n    let x = 1;\n}\n");
    let buffer = match h.request(Request::OpenBuffer {
        workspace,
        path: path.clone(),
    }) {
        Ok(Response::Buffer(snapshot)) => {
            assert_eq!(snapshot.language.as_deref(), Some("rust"));
            assert!(snapshot.text.contains("fn main"));
            snapshot
        }
        other => panic!("バッファを開けない: {other:?}"),
    };

    // ハイライトが返る (Tree-sitter がバックエンドで動いていることの確認)
    match h.request(Request::RequestHighlights {
        buffer: buffer.id,
        start_row: 0,
        end_row: 3,
    }) {
        Ok(Response::Highlights { spans, version, .. }) => {
            assert_eq!(version, buffer.version);
            assert!(!spans.is_empty(), "ハイライトが 1 つも返らない");
        }
        other => panic!("ハイライトを取得できない: {other:?}"),
    }

    // 編集する
    let version = match h.request(Request::ApplyEdits {
        buffer: buffer.id,
        base_version: buffer.version,
        edits: vec![Edit::insert(0, "// 先頭コメント\n")],
    }) {
        Ok(Response::BufferVersion { version }) => {
            assert!(version > buffer.version);
            version
        }
        other => panic!("編集を適用できない: {other:?}"),
    };

    // 古い版数での編集は拒否される
    let stale = h.request(Request::ApplyEdits {
        buffer: buffer.id,
        base_version: buffer.version,
        edits: vec![Edit::insert(0, "x")],
    });
    assert!(stale.is_err(), "古い版数の編集が通ってしまった");

    // 保存してディスクに反映される
    match h.request(Request::SaveBuffer {
        buffer: buffer.id,
        path: None,
    }) {
        Ok(Response::Saved { path: saved, .. }) => assert_eq!(saved, path),
        other => panic!("保存できない: {other:?}"),
    }
    let on_disk = std::fs::read_to_string(&path).expect("保存後の読み出し");
    assert!(
        on_disk.starts_with("// 先頭コメント"),
        "ディスクの内容に編集が反映されていない: {on_disk:?}"
    );

    // 版数は保存後も進んでいる
    assert!(version > 0);
}

#[test]
fn ディレクトリ一覧が取得できる() {
    let mut h = Harness::start("readdir");
    let workspace = match h.request(Request::OpenWorkspace {
        root: h.root().to_path_buf(),
    }) {
        Ok(Response::Workspace(info)) => info.id,
        other => panic!("ワークスペースを開けない: {other:?}"),
    };
    h.write_file("a.txt", "");
    h.write_file("sub/b.txt", "");

    match h.request(Request::ReadDir {
        workspace,
        path: h.root().to_path_buf(),
    }) {
        Ok(Response::DirEntries(entries)) => {
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"a.txt"), "{names:?}");
            assert!(names.contains(&"sub"), "{names:?}");
            // フォルダが先に並ぶ
            assert_eq!(entries[0].name, "sub");
        }
        other => panic!("一覧を取得できない: {other:?}"),
    }
}

#[test]
fn 存在しないワークスペースへの要求はエラーになる() {
    let mut h = Harness::start("badws");
    let result = h.request(Request::ReadDir {
        workspace: nebula_protocol::WorkspaceId(999_999),
        path: h.root().to_path_buf(),
    });
    assert!(result.is_err(), "存在しないワークスペースが通ってしまった");
}

#[test]
fn 対応する括弧を返す() {
    let mut h = Harness::start("bracket");
    let workspace = match h.request(Request::OpenWorkspace {
        root: h.root().to_path_buf(),
    }) {
        Ok(Response::Workspace(info)) => info.id,
        other => panic!("{other:?}"),
    };
    let path = h.write_file("b.rs", "fn f() { let a = (1 + 2); }\n");
    let buffer = match h.request(Request::OpenBuffer { workspace, path }) {
        Ok(Response::Buffer(s)) => s,
        other => panic!("{other:?}"),
    };
    // 7 文字目の '{' に対応する '}' は末尾から 2 文字目。
    let text = &buffer.text;
    let open = text.find('{').unwrap();
    let close = text.rfind('}').unwrap();
    match h.request(Request::MatchingBracket {
        buffer: buffer.id,
        offset: open,
    }) {
        Ok(Response::MatchingBracket(Some(found))) => assert_eq!(found, close),
        other => panic!("対応括弧が返らない: {other:?}"),
    }
}

#[test]
fn 一時バッファを作って編集できる() {
    let mut h = Harness::start("scratch");
    let workspace = match h.request(Request::OpenWorkspace {
        root: h.root().to_path_buf(),
    }) {
        Ok(Response::Workspace(info)) => info.id,
        other => panic!("{other:?}"),
    };
    let buffer = match h.request(Request::CreateScratchBuffer {
        workspace,
        language: Some("rust".into()),
    }) {
        Ok(Response::Buffer(s)) => s,
        other => panic!("{other:?}"),
    };
    assert!(buffer.path.is_none());
    assert_eq!(buffer.language.as_deref(), Some("rust"));

    match h.request(Request::ApplyEdits {
        buffer: buffer.id,
        base_version: buffer.version,
        edits: vec![Edit::replace(TextRange::new(0, 0), "fn f() {}")],
    }) {
        Ok(Response::BufferVersion { .. }) => {}
        other => panic!("一時バッファを編集できない: {other:?}"),
    }
}

/// Markdown プレビューの構文解析がバックエンドで行われ、要素列が返ることを確かめる。
///
/// GUI プロセスは tree-sitter を一切動かさない設計 (`ARCHITECTURE.md`) なので、
/// この経路が通しで動くことを IPC 越しに確認しておく。個々のブロック変換の
/// 網羅的なケースは `nebula-core` の単体テスト (`markdown_preview.rs`) 側にある。
#[test]
fn markdownプレビューの要素列が返る() {
    let mut h = Harness::start("mdpreview");
    let workspace = match h.request(Request::OpenWorkspace {
        root: h.root().to_path_buf(),
    }) {
        Ok(Response::Workspace(info)) => info.id,
        other => panic!("{other:?}"),
    };
    let path = h.write_file("note.md", "# 見出し\n\n- 項目1\n- 項目2\n");
    let buffer = match h.request(Request::OpenBuffer { workspace, path }) {
        Ok(Response::Buffer(s)) => s,
        other => panic!("{other:?}"),
    };

    match h.request(Request::MarkdownPreview { buffer: buffer.id }) {
        Ok(Response::MarkdownPreview { version, blocks }) => {
            assert_eq!(version, buffer.version);
            assert_eq!(
                blocks,
                vec![
                    PreviewBlock::Heading {
                        level: 1,
                        text: "見出し".into(),
                    },
                    PreviewBlock::ListItem {
                        depth: 0,
                        marker: ListMarker::Bullet,
                        text: "項目1".into(),
                    },
                    PreviewBlock::ListItem {
                        depth: 0,
                        marker: ListMarker::Bullet,
                        text: "項目2".into(),
                    },
                ]
            );
        }
        other => panic!("プレビューを取得できない: {other:?}"),
    }
}

/// diff ハンクの行番号が 0 始まりで返ることを確かめる。
///
/// git の diff 出力は 1 始まり、プロトコルの `DiffHunk.new_start` は 0 始まりと
/// 定めてある。この境界は git モジュールの単体テストでも GUI の単体テストでも
/// 露見しないので、通しで 1 か所だけ検査する。
#[test]
fn diff_ハンクの行番号が_0_始まりで返る() {
    let mut h = Harness::start("difflines");
    let root = h.root().to_path_buf();

    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        matches!(status, Ok(s) if s.success())
    };
    if !git(&["init", "-q"]) {
        eprintln!("git が使えないため diff の検査を省略します");
        return;
    }
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "nebula-test"]);

    // 5 行のファイルを作ってコミットする。
    let path = h.write_file("lines.txt", "a\nb\nc\nd\ne\n");
    assert!(git(&["add", "lines.txt"]));
    assert!(git(&["commit", "-q", "-m", "init"]));

    // 3 行目 (0 始まりで 2) だけを変える。
    std::fs::write(&path, "a\nb\nCHANGED\nd\ne\n").expect("書き換え");

    let workspace = match h.request(Request::OpenWorkspace { root: root.clone() }) {
        Ok(Response::Workspace(info)) => {
            assert!(
                info.git_root.is_some(),
                "git リポジトリとして認識されていない"
            );
            info.id
        }
        other => panic!("{other:?}"),
    };

    match h.request(Request::GitDiffHunks {
        workspace,
        path: path.clone(),
        contents: None,
    }) {
        Ok(Response::GitHunks(hunks)) => {
            assert_eq!(hunks.len(), 1, "ハンクが 1 つだけ返るはず: {hunks:?}");
            let hunk = &hunks[0];
            assert_eq!(
                hunk.new_start, 2,
                "3 行目の変更は 0 始まりで 2 のはず: {hunk:?}"
            );
            assert_eq!(hunk.new_lines, 1, "{hunk:?}");
        }
        other => panic!("diff を取得できない: {other:?}"),
    }

    // 未保存の内容に対する差分も同じ座標系で返ること。
    match h.request(Request::GitDiffHunks {
        workspace,
        path,
        contents: Some("a\nb\nc\nd\nZZZ\n".to_string()),
    }) {
        Ok(Response::GitHunks(hunks)) => {
            assert_eq!(hunks.len(), 1, "{hunks:?}");
            assert_eq!(
                hunks[0].new_start, 4,
                "5 行目の変更は 0 始まりで 4 のはず: {:?}",
                hunks[0]
            );
        }
        other => panic!("未保存内容の diff を取得できない: {other:?}"),
    }
}

/// ステージしても行ガターの印が消えないことを確かめる。
///
/// 比較の基準をインデックスにしていると、`git add` した瞬間に差分が空になり
/// ガターの印が消える。基準は常に HEAD でなければならない。
#[test]
fn ステージ済みの変更も_diff_ハンクに出る() {
    let mut h = Harness::start("staged");
    let root = h.root().to_path_buf();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !git(&["init", "-q"]) {
        eprintln!("git が使えないため省略します");
        return;
    }
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "nebula-test"]);

    let path = h.write_file("s.txt", "a\nb\nc\n");
    assert!(git(&["add", "."]));
    assert!(git(&["commit", "-q", "-m", "init"]));

    std::fs::write(&path, "a\nCHANGED\nc\n").expect("書き換え");
    assert!(git(&["add", "s.txt"]), "ステージできない");

    let workspace = match h.request(Request::OpenWorkspace { root }) {
        Ok(Response::Workspace(info)) => info.id,
        other => panic!("{other:?}"),
    };
    match h.request(Request::GitDiffHunks {
        workspace,
        path,
        contents: None,
    }) {
        Ok(Response::GitHunks(hunks)) => {
            assert_eq!(hunks.len(), 1, "ステージ後にハンクが消えた: {hunks:?}");
            assert_eq!(hunks[0].new_start, 1, "{:?}", hunks[0]);
        }
        other => panic!("diff を取得できない: {other:?}"),
    }
}
