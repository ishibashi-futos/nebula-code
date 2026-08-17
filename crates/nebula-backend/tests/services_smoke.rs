//! 外部プロセスを伴うサービスの疎通確認。
//!
//! 各サービスの単体テストは出力パーサを対象にしている。ここでは実際に
//! ripgrep・git・PTY・codex を起動し、GUI が使うのと同じ要求経路で
//! 応答が返ることだけを確かめる。

use nebula_backend::{BackendState, ipc, tools};
use nebula_protocol::{
    ClientMessage, Event, FrameDecoder, Request, RequestId, Response, SearchQuery, ServerMessage,
    TerminalSpec, encode_frame,
};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    socket: PathBuf,
    workdir: PathBuf,
    /// 応答を待つ間に届いたイベント。あとから検査する。
    events: Vec<Event>,
}

impl Client {
    fn start(name: &str) -> Self {
        let unique = format!("{}-{}", std::process::id(), name);
        let socket = std::env::temp_dir().join(format!("nebula-sm-{unique}.sock"));
        let workdir = std::env::temp_dir().join(format!("nebula-sm-{unique}"));
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).expect("作業ディレクトリ");

        let socket_for_server = socket.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("ランタイム");
            runtime.block_on(async move {
                let detected = tools::detect_all().await;
                let state = BackendState::new(detected);
                let _ = ipc::serve(&socket_for_server, state).await;
            });
        });

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
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("タイムアウト設定");
        Self {
            stream,
            decoder: FrameDecoder::new(),
            socket,
            workdir,
            events: Vec::new(),
        }
    }

    fn request(&mut self, request: Request) -> Result<Response, String> {
        let id = RequestId::next();
        let frame = encode_frame(&ClientMessage::Request { id, request }).expect("符号化");
        self.stream.write_all(&frame).expect("送信");
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            while let Ok(Some(message)) = self.decoder.next_message::<ServerMessage>() {
                match message {
                    ServerMessage::Response {
                        id: got,
                        result,
                    } if got == id => return result.map_err(|e| e.to_string()),
                    ServerMessage::Response { .. } => {}
                    ServerMessage::Event(event) => self.events.push(event),
                }
            }
            let read = self.stream.read(&mut chunk).expect("受信");
            assert!(read > 0, "接続が切れた");
            self.decoder.feed(&chunk[..read]);
        }
    }

    /// 条件を満たすイベントが届くまで待つ。
    fn wait_event(&mut self, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        if self.events.iter().any(&mut pred) {
            return true;
        }
        let mut chunk = vec![0u8; 64 * 1024];
        self.stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .ok();
        while Instant::now() < deadline {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => self.decoder.feed(&chunk[..read]),
                Err(_) => continue,
            }
            while let Ok(Some(message)) = self.decoder.next_message::<ServerMessage>() {
                if let ServerMessage::Event(event) = message {
                    let matched = pred(&event);
                    self.events.push(event);
                    if matched {
                        self.stream
                            .set_read_timeout(Some(Duration::from_secs(30)))
                            .ok();
                        return true;
                    }
                }
            }
        }
        self.stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .ok();
        false
    }

    fn open_workspace(&mut self, root: PathBuf) -> nebula_protocol::WorkspaceId {
        match self.request(Request::OpenWorkspace { root }) {
            Ok(Response::Workspace(info)) => info.id,
            other => panic!("ワークスペースを開けない: {other:?}"),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.request(Request::Shutdown);
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

fn has(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(tool).is_file())
        })
        .unwrap_or(false)
}

#[test]
fn ripgrep_検索の結果がイベントで届く() {
    if !has("rg") {
        eprintln!("ripgrep が無いため省略");
        return;
    }
    let mut c = Client::start("search");
    let root = c.workdir.clone();
    std::fs::write(root.join("a.rs"), "fn 探索対象() {}\nfn other() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "// 探索対象 をコメントで\n").unwrap();
    let workspace = c.open_workspace(root);

    let search = match c.request(Request::StartSearch {
        workspace,
        query: SearchQuery {
            pattern: "探索対象".into(),
            ..SearchQuery::default()
        },
    }) {
        Ok(Response::SearchStarted { search }) => search,
        other => panic!("検索を開始できない: {other:?}"),
    };

    let finished = c.wait_event(Duration::from_secs(15), |event| {
        matches!(event, Event::SearchFinished { search: s, .. } if *s == search)
    });
    assert!(finished, "検索完了イベントが届かない");

    let total: usize = c
        .events
        .iter()
        .filter_map(|e| match e {
            Event::SearchMatches { matches, .. } => Some(matches.len()),
            _ => None,
        })
        .sum();
    assert_eq!(total, 2, "2 ファイルで 1 件ずつ見つかるはず");

    // 一致位置が文字オフセットで返ること (バイトのままだと日本語でずれる)。
    let first = c
        .events
        .iter()
        .find_map(|e| match e {
            Event::SearchMatches { matches, .. } => matches.first().cloned(),
            _ => None,
        })
        .expect("一致が 1 件も無い");
    let line_chars: Vec<char> = first.line_text.chars().collect();
    let range = first.matches[0];
    let matched: String = line_chars[range.start..range.end].iter().collect();
    assert_eq!(matched, "探索対象", "文字オフセットがずれている");
}

#[test]
fn ファイルのあいまい検索が候補を返す() {
    let mut c = Client::start("findfiles");
    let root = c.workdir.clone();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join("src/deep/editor_view.rs"), "").unwrap();
    std::fs::write(root.join("src/other.rs"), "").unwrap();
    let workspace = c.open_workspace(root);

    match c.request(Request::FindFiles {
        workspace,
        query: "edview".into(),
        limit: 10,
    }) {
        Ok(Response::FileCandidates(candidates)) => {
            assert!(!candidates.is_empty(), "候補が 0 件");
            assert!(
                candidates[0].relative.contains("editor_view"),
                "期待した候補が先頭に来ない: {:?}",
                candidates.iter().map(|c| &c.relative).collect::<Vec<_>>()
            );
        }
        other => panic!("あいまい検索に失敗: {other:?}"),
    }
}

#[test]
fn git_の状態とブランチが取得できる() {
    if !has("git") {
        eprintln!("git が無いため省略");
        return;
    }
    let mut c = Client::start("git");
    let root = c.workdir.clone();
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
    assert!(git(&["init", "-q"]));
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "nebula"]);
    std::fs::write(root.join("committed.txt"), "old\n").unwrap();
    assert!(git(&["add", "."]));
    assert!(git(&["commit", "-q", "-m", "初回"]));
    std::fs::write(root.join("committed.txt"), "new\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "x\n").unwrap();

    let workspace = c.open_workspace(root.clone());

    match c.request(Request::GitStatus { workspace }) {
        Ok(Response::GitStatus(status)) => {
            assert!(status.branch.is_some(), "ブランチ名が取れない");
            let names: Vec<String> = status
                .entries
                .iter()
                .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert!(names.contains(&"committed.txt".to_string()), "{names:?}");
            assert!(names.contains(&"untracked.txt".to_string()), "{names:?}");
        }
        other => panic!("git status に失敗: {other:?}"),
    }

    match c.request(Request::GitListBranches { workspace }) {
        Ok(Response::GitBranches(branches)) => {
            assert_eq!(branches.iter().filter(|b| b.is_head).count(), 1);
        }
        other => panic!("ブランチ一覧に失敗: {other:?}"),
    }

    match c.request(Request::GitBlame {
        workspace,
        path: root.join("committed.txt"),
    }) {
        Ok(Response::GitBlame(lines)) => assert_eq!(lines.len(), 1),
        other => panic!("blame に失敗: {other:?}"),
    }

    // ステージしてコミットできる
    assert!(
        c.request(Request::GitStage {
            workspace,
            paths: vec![root.join("committed.txt")],
        })
        .is_ok()
    );
    assert!(
        c.request(Request::GitCommit {
            workspace,
            message: "2 回目".into(),
            amend: false,
        })
        .is_ok()
    );
    match c.request(Request::GitLog {
        workspace,
        path: None,
        limit: 10,
    }) {
        Ok(Response::GitLog(commits)) => {
            assert_eq!(commits.len(), 2, "コミットが 2 件あるはず");
            assert_eq!(commits[0].summary, "2 回目");
        }
        other => panic!("log に失敗: {other:?}"),
    }
}

#[test]
fn ターミナルが起動して出力を返す() {
    let mut c = Client::start("term");
    let root = c.workdir.clone();
    let workspace = c.open_workspace(root.clone());

    let terminal = match c.request(Request::TerminalCreate {
        spec: TerminalSpec {
            workspace,
            shell: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'NEBULA_OK'; sleep 1".into()],
            cwd: Some(root),
            env: Vec::new(),
            rows: 24,
            cols: 80,
        },
    }) {
        Ok(Response::Terminal { terminal }) => terminal,
        other => panic!("ターミナルを起動できない: {other:?}"),
    };

    let seen = c.wait_event(Duration::from_secs(10), |event| match event {
        Event::TerminalUpdated(update) if update.id == terminal => update
            .dirty_lines
            .iter()
            .any(|(_, cells)| cells.iter().map(|c| c.ch).collect::<String>().contains("NEBULA_OK")),
        _ => false,
    });
    assert!(seen, "ターミナルの出力が届かない");

    // 終了イベントも届く
    let exited = c.wait_event(Duration::from_secs(10), |event| {
        matches!(event, Event::TerminalExited { terminal: t, .. } if *t == terminal)
    });
    assert!(exited, "ターミナル終了イベントが届かない");
}

#[test]
fn codex_の状態問い合わせが応答する() {
    if !has("codex") {
        eprintln!("codex が無いため省略");
        return;
    }
    let mut c = Client::start("codex");
    // 認証状態はログインの有無にかかわらず応答が返るべき。
    match c.request(Request::CodexAuthStatus) {
        Ok(Response::CodexAuth { .. }) => {}
        other => panic!("認証状態を取得できない: {other:?}"),
    }
}
