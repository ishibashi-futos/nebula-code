//! IPC で交換されるメッセージ本体。
//!
//! 3 種類しかない:
//! - [`Request`]  … GUI → バックエンド。必ず 1 つの [`Response`] が返る。
//! - [`Response`] … バックエンド → GUI。要求と 1 対 1 に対応する。
//! - [`Event`]    … バックエンド → GUI。要求と対応しない非同期通知。
//!
//! ストリーミングする結果 (検索・ターミナル出力・Codex 応答) は、開始要求に対して
//! 即座に ID を返し、以降の増分は [`Event`] で流す。これにより GUI 側は結果全体を
//! 待たずに描画を始められる。

use crate::error::ProtocolError;
use crate::ids::*;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// GUI → バックエンド。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Request {
        id: RequestId,
        request: Request,
    },
    /// 進行中の要求の取り消し。応答は返らないこともある。
    Cancel {
        id: RequestId,
    },
}

/// バックエンド → GUI。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    Response {
        id: RequestId,
        result: Result<Response, ProtocolError>,
    },
    Event(Event),
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    // -- 接続 --
    /// 接続直後に必ず送る。版数不一致ならバックエンドはエラーを返して切断する。
    Handshake {
        protocol_version: u32,
    },
    /// バックエンドを終了させる。最後の GUI が閉じるときに送る。
    Shutdown,

    // -- ワークスペース --
    OpenWorkspace {
        root: PathBuf,
    },
    CloseWorkspace {
        workspace: WorkspaceId,
    },
    ListWorkspaces,

    // -- ファイルシステム --
    ReadDir {
        workspace: WorkspaceId,
        path: PathBuf,
    },
    CreateFile {
        path: PathBuf,
    },
    CreateDir {
        path: PathBuf,
    },
    RenamePath {
        from: PathBuf,
        to: PathBuf,
    },
    /// ゴミ箱ではなく即時削除する。GUI 側で確認を取る前提。
    DeletePath {
        path: PathBuf,
        recursive: bool,
    },
    CopyPath {
        from: PathBuf,
        to: PathBuf,
    },
    /// エディタ外からの変更を監視する。ワークスペースを開いた時点で自動的に開始される。
    WatchPath {
        workspace: WorkspaceId,
        path: PathBuf,
    },

    // -- バッファ --
    OpenBuffer {
        workspace: WorkspaceId,
        path: PathBuf,
    },
    /// ファイルに紐づかない一時バッファを作る。
    CreateScratchBuffer {
        workspace: WorkspaceId,
        language: Option<String>,
    },
    CloseBuffer {
        buffer: BufferId,
    },
    /// 編集を適用する。`base_version` が現在の版と一致しない場合は競合エラー。
    ApplyEdits {
        buffer: BufferId,
        base_version: u64,
        edits: Vec<Edit>,
    },
    SaveBuffer {
        buffer: BufferId,
        /// 別名保存。`None` なら元のパスに書く。
        path: Option<PathBuf>,
    },
    /// 指定行範囲のハイライトを要求する。GUI は可視範囲だけを要求する。
    RequestHighlights {
        buffer: BufferId,
        start_row: u32,
        end_row: u32,
    },
    /// バッファ内の対応する括弧を求める。
    MatchingBracket {
        buffer: BufferId,
        offset: usize,
    },
    /// インデント幅など言語ごとの設定を取得する。
    BufferLanguageConfig {
        buffer: BufferId,
    },
    /// Markdown プレビュー用の要素列を要求する。構文解析はバックエンドが行うので、
    /// 内容は送らず `buffer` だけで指定する (バックエンドが正本を持っている)。
    MarkdownPreview {
        buffer: BufferId,
    },

    // -- 検索 --
    /// ワークスペース全文検索を開始する。結果は [`Event::SearchMatches`] で流れる。
    StartSearch {
        workspace: WorkspaceId,
        query: SearchQuery,
    },
    CancelSearch {
        search: SearchId,
    },
    /// ファイル名のあいまい検索 (クイックオープン)。件数が少ないので同期的に返す。
    FindFiles {
        workspace: WorkspaceId,
        query: String,
        limit: usize,
    },
    /// 検索結果の一括置換。
    ReplaceAll {
        workspace: WorkspaceId,
        query: SearchQuery,
        replacement: String,
    },

    // -- Git --
    GitStatus {
        workspace: WorkspaceId,
    },
    /// 作業ツリーと HEAD の差分ハンク。行ガター表示に使う。
    GitDiffHunks {
        workspace: WorkspaceId,
        path: PathBuf,
        /// 未保存の内容に対する差分を取る場合に渡す。`None` ならディスク上の内容を使う。
        contents: Option<String>,
    },
    GitBlame {
        workspace: WorkspaceId,
        path: PathBuf,
    },
    GitStage {
        workspace: WorkspaceId,
        paths: Vec<PathBuf>,
    },
    GitUnstage {
        workspace: WorkspaceId,
        paths: Vec<PathBuf>,
    },
    GitDiscardChanges {
        workspace: WorkspaceId,
        paths: Vec<PathBuf>,
    },
    GitCommit {
        workspace: WorkspaceId,
        message: String,
        amend: bool,
    },
    GitListBranches {
        workspace: WorkspaceId,
    },
    GitCheckout {
        workspace: WorkspaceId,
        branch: String,
        create: bool,
    },
    GitLog {
        workspace: WorkspaceId,
        path: Option<PathBuf>,
        limit: usize,
    },
    /// HEAD 時点のファイル内容。差分ビューの左側に使う。
    GitFileAtHead {
        workspace: WorkspaceId,
        path: PathBuf,
    },
    GitPush {
        workspace: WorkspaceId,
    },
    GitPull {
        workspace: WorkspaceId,
    },

    // -- LSP --
    /// バッファの言語に対応する言語サーバーを起動する (未起動なら)。
    LspEnsureServer {
        buffer: BufferId,
    },
    LspHover {
        buffer: BufferId,
        position: Position,
    },
    LspCompletion {
        buffer: BufferId,
        position: Position,
        /// 補完を誘発した文字 (`.` など)。
        trigger: Option<String>,
    },
    LspDefinition {
        buffer: BufferId,
        position: Position,
    },
    LspReferences {
        buffer: BufferId,
        position: Position,
    },
    LspDocumentSymbols {
        buffer: BufferId,
    },
    LspFormat {
        buffer: BufferId,
    },
    LspRename {
        buffer: BufferId,
        position: Position,
        new_name: String,
    },
    LspSignatureHelp {
        buffer: BufferId,
        position: Position,
    },
    LspCodeActions {
        buffer: BufferId,
        range: SpanRange,
    },
    LspApplyCodeAction {
        buffer: BufferId,
        /// [`Response::CodeActions`] で返した索引。
        index: usize,
    },
    LspServerStatuses {
        workspace: WorkspaceId,
    },
    LspRestartServer {
        workspace: WorkspaceId,
        language: String,
    },

    // -- ターミナル --
    TerminalCreate {
        spec: TerminalSpec,
    },
    TerminalInput {
        terminal: TerminalId,
        /// キー入力をエスケープシーケンス化した生バイト列。
        bytes: Vec<u8>,
    },
    TerminalResize {
        terminal: TerminalId,
        rows: u16,
        cols: u16,
    },
    TerminalScroll {
        terminal: TerminalId,
        /// 正で過去方向 (上) へ、負で現在方向 (下) へ。
        delta_lines: i32,
    },
    TerminalClose {
        terminal: TerminalId,
    },

    // -- Codex --
    CodexNewConversation {
        spec: CodexSessionSpec,
    },
    CodexSendMessage {
        conversation: CodexConversationId,
        text: String,
        /// 参照させるファイル。エディタで開いている内容を添付する用途。
        attachments: Vec<PathBuf>,
    },
    CodexRespondApproval {
        conversation: CodexConversationId,
        request_id: String,
        decision: CodexApprovalDecision,
    },
    CodexInterrupt {
        conversation: CodexConversationId,
    },
    CodexCloseConversation {
        conversation: CodexConversationId,
    },
    CodexListModels,
    /// ログイン状態の確認。未ログインなら GUI が案内を出す。
    CodexAuthStatus,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// 副作用のみで返す値がない要求への応答。
    Ack,
    Handshake(HandshakeInfo),

    Workspace(WorkspaceInfo),
    Workspaces(Vec<WorkspaceInfo>),

    DirEntries(Vec<DirEntry>),

    Buffer(BufferSnapshot),
    /// 編集適用後の新しい版数。
    BufferVersion {
        version: u64,
    },
    /// 保存後の状態。
    Saved {
        path: PathBuf,
        version: u64,
    },
    Highlights {
        /// 応答が対応するバッファ版数。GUI は古い応答を捨てる。
        version: u64,
        start_row: u32,
        end_row: u32,
        spans: Vec<HighlightSpan>,
    },
    MatchingBracket(Option<usize>),
    LanguageConfig(LanguageConfig),
    /// Markdown プレビュー用の要素列。
    MarkdownPreview {
        /// 応答が対応するバッファ版数。GUI は古い応答を捨てる。
        version: u64,
        blocks: Vec<PreviewBlock>,
    },

    SearchStarted {
        search: SearchId,
    },
    FileCandidates(Vec<FileCandidate>),
    ReplaceResult {
        files_changed: usize,
        replacements: usize,
    },

    GitStatus(GitRepoStatus),
    GitHunks(Vec<DiffHunk>),
    GitBlame(Vec<BlameLine>),
    GitBranches(Vec<GitBranch>),
    GitLog(Vec<GitCommitInfo>),
    GitFileContents(String),

    Hover(Option<HoverInfo>),
    Completions(Vec<CompletionItem>),
    Locations(Vec<LocationLink>),
    DocumentSymbols(Vec<SymbolInfo>),
    TextEdits(Vec<TextEditOp>),
    WorkspaceEdit(WorkspaceEdit),
    SignatureHelp(Option<SignatureHelp>),
    CodeActions(Vec<CodeAction>),
    LspServerStatuses(Vec<LspServerStatus>),

    Terminal {
        terminal: TerminalId,
    },

    CodexConversation {
        conversation: CodexConversationId,
    },
    CodexModels(Vec<String>),
    CodexAuth {
        logged_in: bool,
        account: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
}

/// 言語ごとの編集設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageConfig {
    pub language: Option<String>,
    pub indent_width: u32,
    pub use_tabs: bool,
    /// 行コメントの接頭辞 (`//` など)。
    pub line_comment: Option<String>,
    /// ブロックコメントの開始・終了。
    pub block_comment: Option<(String, String)>,
    /// 自動閉じ括弧の組。
    pub auto_close_pairs: Vec<(String, String)>,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            language: None,
            indent_width: 4,
            use_tabs: false,
            line_comment: None,
            block_comment: None,
            auto_close_pairs: vec![
                ("(".into(), ")".into()),
                ("[".into(), "]".into()),
                ("{".into(), "}".into()),
                ("\"".into(), "\"".into()),
                ("'".into(), "'".into()),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// 検索結果の増分。
    SearchMatches {
        search: SearchId,
        matches: Vec<SearchMatch>,
    },
    SearchFinished {
        search: SearchId,
        total: usize,
        /// `max_results` に達して打ち切ったか。
        truncated: bool,
    },

    /// エディタ外でのファイル変更。
    FilesChanged {
        workspace: WorkspaceId,
        changes: Vec<FileChange>,
    },
    /// 開いているバッファの元ファイルが外部で変更された。
    BufferFileChanged {
        buffer: BufferId,
        /// バックエンドが再読込済みの新しい内容。GUI が未編集ならそのまま差し替える。
        new_text: String,
        version: u64,
    },

    /// ハイライトが再計算された (編集後の非同期更新)。
    HighlightsInvalidated {
        buffer: BufferId,
        version: u64,
    },

    GitStatusChanged {
        workspace: WorkspaceId,
        status: GitRepoStatus,
    },

    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    LspStatusChanged {
        workspace: WorkspaceId,
        status: LspServerStatus,
    },
    /// 言語サーバーからのログ・進捗通知。
    LspMessage {
        language: String,
        message: String,
    },

    TerminalUpdated(TerminalUpdate),
    TerminalExited {
        terminal: TerminalId,
        exit_code: Option<i32>,
    },

    Codex(CodexEvent),

    /// 外部ツールの検出が完了した。
    ///
    /// 検出はソケットを開いた後に非同期で走るため、ハンドシェイク応答の時点では
    /// 空 (未検出) のことがある。GUI はこのイベントで確定値に差し替える。
    ToolsDetected {
        tools: DetectedTools,
    },

    /// バックエンド側の致命的でない不具合。GUI は通知として出す。
    Notification {
        level: NotificationLevel,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}
