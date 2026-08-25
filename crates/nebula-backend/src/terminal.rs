//! 統合ターミナル。
//!
//! PTY を開いて出力を [`grid`] のエミュレータに食わせ、変化した行だけを
//! `Event::TerminalUpdated` で GUI へ流す。
//!
//! セッションごとに OS スレッドを 2 本使う。
//!
//! - 読み取りスレッド: PTY からの `read` でブロックし、届いたバイト列を解釈する。
//!   `read` にはタイムアウトが無いので、tokio のタスクに載せると実行枠を占有してしまう。
//! - 送出スレッド: 16ms 周期で差分を取り出して送る。読み取りのたびに送ると、
//!   1 バイトずつ届く対話入力で IPC が溢れてエディタが止まる。
//!
//! シェルを明示指定されていないときは、一般的な端末エミュレータと同じくログインシェル
//! として起動する。そうしないとログインシェル用の設定 (zsh なら `.zprofile` / `.zlogin`)
//! が読まれず、プロンプトやパスの設定が反映されない。

mod grid;

use grid::TerminalEmulator;
use nebula_protocol::{Event, ProtocolError, TerminalId, TerminalSpec};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// GUI へ差分を送る間隔。60Hz 相当。
const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// 起動中の PTY セッション 1 つ。
struct Session {
    emulator: Arc<Mutex<TerminalEmulator>>,
    /// PTY への書き込み口。`take_writer` は 1 度しか呼べないので生成時に確保しておく。
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

type Sessions = Arc<Mutex<HashMap<TerminalId, Session>>>;

pub struct TerminalService {
    events: broadcast::Sender<Event>,
    /// 読み取りスレッドが終了時に自分の登録を消せるよう `Arc` で共有する。
    sessions: Sessions,
}

impl TerminalService {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            events,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn create(&self, spec: TerminalSpec) -> Result<TerminalId, ProtocolError> {
        let rows = spec.rows.max(1);
        let cols = spec.cols.max(1);

        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .map_err(|e| ProtocolError::io(format!("PTY を開けません: {e}")))?;

        let child = pair
            .slave
            .spawn_command(build_command(&spec))
            .map_err(|e| ProtocolError::io(format!("シェルを起動できません: {e}")))?;
        // slave 側の fd を握ったままだと、子が終了しても master の読み取りが EOF にならない。
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ProtocolError::io(format!("PTY を読めません: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ProtocolError::io(format!("PTY へ書き込めません: {e}")))?;

        let id = TerminalId::next();
        let emulator = Arc::new(Mutex::new(TerminalEmulator::new(rows, cols)));
        let writer = Arc::new(Mutex::new(writer));
        self.sessions
            .lock()
            .expect("ターミナル一覧のロック")
            .insert(
                id,
                Session {
                    emulator: Arc::clone(&emulator),
                    writer: Arc::clone(&writer),
                    killer: child.clone_killer(),
                    master: pair.master,
                },
            );

        let finished = Arc::new(AtomicBool::new(false));
        spawn_reader(
            id,
            reader,
            child,
            Arc::clone(&emulator),
            writer,
            Arc::clone(&self.sessions),
            self.events.clone(),
            Arc::clone(&finished),
        );
        spawn_flusher(id, emulator, self.events.clone(), finished);
        Ok(id)
    }

    pub fn input(&self, terminal: TerminalId, bytes: &[u8]) -> Result<(), ProtocolError> {
        let (writer, emulator) = self.with_session(terminal, |s| {
            (Arc::clone(&s.writer), Arc::clone(&s.emulator))
        })?;

        let bytes = {
            let mut emulator = emulator.lock().expect("ターミナルの解釈のロック");
            let grid = emulator.grid_mut();
            // 履歴を遡って見ている最中に打鍵したら最下部へ戻す。端末の慣習に合わせる。
            grid.scroll_to_bottom();
            // GUI は端末モードを知らないので、DECCKM によるカーソルキーの形の違いは
            // ここで吸収する。GUI へモードを伝えるには `TerminalUpdate` の拡張が要る。
            grid.encode_input(bytes).into_owned()
        };

        let mut writer = writer.lock().expect("PTY 書き込みのロック");
        writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .map_err(|e| ProtocolError::io(format!("ターミナル {terminal} へ書き込めません: {e}")))
    }

    pub fn resize(&self, terminal: TerminalId, rows: u16, cols: u16) -> Result<(), ProtocolError> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let (resized, emulator) = self.with_session(terminal, |s| {
            (
                s.master.resize(pty_size(rows, cols)),
                Arc::clone(&s.emulator),
            )
        })?;
        resized.map_err(|e| ProtocolError::io(format!("PTY をリサイズできません: {e}")))?;

        emulator
            .lock()
            .expect("ターミナルの解釈のロック")
            .grid_mut()
            .resize(rows, cols);
        Ok(())
    }

    pub fn scroll(&self, terminal: TerminalId, delta_lines: i32) -> Result<(), ProtocolError> {
        let emulator = self.with_session(terminal, |s| Arc::clone(&s.emulator))?;
        emulator
            .lock()
            .expect("ターミナルの解釈のロック")
            .grid_mut()
            .scroll_view(delta_lines);
        // スクロールは操作への直接の応答なので、送出スレッドの周期を待たずに送る。
        flush(&self.events, terminal, &emulator);
        Ok(())
    }

    pub fn close(&self, terminal: TerminalId) -> Result<(), ProtocolError> {
        // 子が自然終了した直後に GUI が閉じる操作をする競合は普通に起きる。
        // 未知の ID をエラーにすると、その競合が毎回エラー表示になってしまう。
        // `TerminalExited` は読み取りスレッドが必ず送るので、ここでは送らない。
        if let Some(session) = self
            .sessions
            .lock()
            .expect("ターミナル一覧のロック")
            .get_mut(&terminal)
        {
            let _ = session.killer.kill();
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let mut sessions = self.sessions.lock().expect("ターミナル一覧のロック");
        for session in sessions.values_mut() {
            let _ = session.killer.kill();
        }
        // master と writer を落とすと、読み取りスレッドが EOF を受けて自分で後始末する。
        sessions.clear();
    }

    /// セッションを引いて閉じた操作を行う。存在しなければエラー。
    fn with_session<T>(
        &self,
        terminal: TerminalId,
        f: impl FnOnce(&Session) -> T,
    ) -> Result<T, ProtocolError> {
        self.sessions
            .lock()
            .expect("ターミナル一覧のロック")
            .get(&terminal)
            .map(f)
            .ok_or_else(|| {
                ProtocolError::not_found(format!("ターミナル {terminal} は開かれていません"))
            })
    }
}

const fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn build_command(spec: &TerminalSpec) -> CommandBuilder {
    // 指定が無ければ既定のシェルに任せる。argv[0] が `-zsh` の形になりログインシェルとして
    // 起動するので、ユーザーの `.zprofile` が読まれる。シェルの決定 (`$SHELL` → passwd db) も
    // portable-pty 側が行うため、こちらでフォールバックを持たない。
    let mut cmd = match &spec.shell {
        Some(shell) => {
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(&spec.args);
            cmd
        }
        // 既定シェルのビルダーには引数を足せない。GUI からの通常起動は引数を渡さない。
        None => CommandBuilder::new_default_prog(),
    };
    if let Some(cwd) = &spec.cwd {
        cmd.cwd(cwd);
    }
    // グリッドが 256 色と一般的な CSI を解釈できることを子プロセスへ伝える。
    // spec.env より先に置いて、呼び出し側が上書きできるようにする。
    cmd.env("TERM", "xterm-256color");
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    cmd
}

/// PTY を読んでエミュレータへ流し続ける。終了時に片付けと `TerminalExited` を担う。
///
/// `writer` は端末クエリ (DSR / DA) への応答を書き戻すために持つ。応答が遅れると
/// カーソル位置を尋ねるプロンプトが待ち続けるので、送出スレッドの周期には載せない。
fn spawn_reader(
    id: TerminalId,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    emulator: Arc<Mutex<TerminalEmulator>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    sessions: Sessions,
    events: broadcast::Sender<Event>,
    finished: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // 解釈のロックを持ったまま PTY へ書かない。書き込みが詰まると
                    // 読み取りごと止まり、子プロセスと相互に待ち合う。
                    let responses = {
                        let mut emulator = emulator.lock().expect("ターミナルの解釈のロック");
                        emulator.advance(&buf[..n]);
                        emulator.grid_mut().take_responses()
                    };
                    if !responses.is_empty() {
                        let mut writer = writer.lock().expect("PTY 書き込みのロック");
                        let _ = writer.write_all(&responses).and_then(|_| writer.flush());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // 子の終了時、環境によっては EOF ではなく EIO が返る。
                Err(_) => break,
            }
        }

        finished.store(true, Ordering::Release);
        // 末尾の出力を届けてから終了を伝える。逆順だと GUI が最後の行を取りこぼす。
        flush(&events, id, &emulator);
        let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
        sessions.lock().expect("ターミナル一覧のロック").remove(&id);
        let _ = events.send(Event::TerminalExited {
            terminal: id,
            exit_code,
        });
    });
}

/// 16ms 周期で差分を送る。
fn spawn_flusher(
    id: TerminalId,
    emulator: Arc<Mutex<TerminalEmulator>>,
    events: broadcast::Sender<Event>,
    finished: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        while !finished.load(Ordering::Acquire) {
            std::thread::sleep(FLUSH_INTERVAL);
            flush(&events, id, &emulator);
        }
    });
}

fn flush(events: &broadcast::Sender<Event>, id: TerminalId, emulator: &Mutex<TerminalEmulator>) {
    // ロックを握ったまま送信しない。購読側の処理時間が PTY の読み取りを止めてしまう。
    let update = emulator
        .lock()
        .expect("ターミナルの解釈のロック")
        .grid_mut()
        .take_update(id);
    if let Some(update) = update {
        let _ = events.send(Event::TerminalUpdated(update));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::WorkspaceId;

    fn spec(shell: &str, args: &[&str]) -> TerminalSpec {
        TerminalSpec {
            workspace: WorkspaceId(1),
            shell: Some(shell.to_string()),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            env: Vec::new(),
            rows: 10,
            cols: 40,
        }
    }

    #[test]
    fn 明示指定が無ければログインシェルとして起動する() {
        let explicit = build_command(&spec("/bin/sh", &["-c", "true"]));
        assert_eq!(explicit.get_argv()[0], "/bin/sh");
        assert_eq!(explicit.get_env("TERM").unwrap(), "xterm-256color");

        let mut inherited = spec("", &[]);
        inherited.shell = None;
        let inherited = build_command(&inherited);
        // 既定シェルのビルダーは argv を持たず、起動時に `-zsh` の形へ組み立てられる。
        assert!(inherited.is_default_prog());
        assert_eq!(inherited.get_env("TERM").unwrap(), "xterm-256color");
    }

    #[test]
    fn 未知のターミナルへの操作はエラーになる() {
        let (events, _rx) = broadcast::channel(16);
        let service = TerminalService::new(events);
        let unknown = TerminalId(9999);
        assert!(service.input(unknown, b"a").is_err());
        assert!(service.resize(unknown, 10, 10).is_err());
        assert!(service.scroll(unknown, 1).is_err());
        // 閉じる操作だけは、自然終了との競合を握りつぶすため成功させる。
        assert!(service.close(unknown).is_ok());
    }

    /// 出力に `needle` が現れ、かつ終了イベントが届くまでイベントを集めて終了コードを返す。
    ///
    /// 出力と終了のどちらが先に届くかは環境で変わるので、片方だけで打ち切らない。
    async fn wait_for_output(
        rx: &mut broadcast::Receiver<Event>,
        id: TerminalId,
        needle: &str,
    ) -> Option<i32> {
        let mut text = String::new();
        let mut exited = None;
        let collect = async {
            while !text.contains(needle) || exited.is_none() {
                match rx.recv().await.expect("イベントを受け取れる") {
                    Event::TerminalUpdated(update) => {
                        assert_eq!(update.id, id);
                        for (_, cells) in &update.dirty_lines {
                            text.extend(cells.iter().map(|c| c.ch));
                        }
                    }
                    Event::TerminalExited {
                        terminal,
                        exit_code,
                    } => {
                        assert_eq!(terminal, id);
                        exited = Some(exit_code);
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), collect)
            .await
            .unwrap_or_else(|_| {
                panic!("{needle:?} が届かない: 受信済み {text:?} / 終了 {exited:?}")
            });
        exited.expect("終了コードが揃っている")
    }

    #[tokio::test]
    async fn pty_で起動した_echo_の出力と終了が届く() {
        let (events, mut rx) = broadcast::channel(256);
        let service = TerminalService::new(events);
        let id = service
            .create(spec("/bin/echo", &["hello"]))
            .expect("PTY を開ける");

        assert_eq!(wait_for_output(&mut rx, id, "hello").await, Some(0));
        service.shutdown();
    }

    #[tokio::test]
    async fn 入力した内容が子プロセスへ届く() {
        let (events, mut rx) = broadcast::channel(256);
        let service = TerminalService::new(events);
        let id = service
            .create(spec("/bin/sh", &["-c", "read line; echo got=$line"]))
            .expect("PTY を開ける");

        service.resize(id, 12, 30).expect("リサイズできる");
        service.input(id, b"nebula\n").expect("書き込める");

        assert_eq!(wait_for_output(&mut rx, id, "got=nebula").await, Some(0));
        service.shutdown();
    }
}
