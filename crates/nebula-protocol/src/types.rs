//! GUI とバックエンドで共有するデータ型。
//!
//! LSP や git、ripgrep の生の型をそのまま流用せず、Nebula 独自の最小限の形に落としている。
//! GUI 側にそれらのクレートを持ち込まないことが目的で、GUI プロセスを軽く保つ設計の一部。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// テキスト位置
// ---------------------------------------------------------------------------

/// 行と桁による位置。桁は行内の **文字 (char) 単位** のオフセット。
///
/// バイトでも UTF-16 でもなく char を採る理由は、ropey の索引単位と一致し、
/// GUI 側で追加の変換なしにカーソル移動を扱えるため。LSP との UTF-16 変換は
/// バックエンドの LSP 層に閉じ込める。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Position {
    pub row: u32,
    pub column: u32,
}

impl Position {
    pub const fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

/// 行桁による範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpanRange {
    pub start: Position,
    pub end: Position,
}

impl SpanRange {
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// バッファ全体の先頭からの文字オフセットによる範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn empty(at: usize) -> Self {
        Self { start: at, end: at }
    }

    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub const fn len(&self) -> usize {
        self.end - self.start
    }
}

/// 1 回の編集操作。`range` を `text` で置換する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: TextRange,
    pub text: String,
}

impl Edit {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: TextRange::empty(at),
            text: text.into(),
        }
    }

    pub fn delete(range: TextRange) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }

    pub fn replace(range: TextRange, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// バッファ
// ---------------------------------------------------------------------------

/// バッファを開いた直後に GUI へ渡す情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BufferSnapshot {
    pub id: BufferId,
    pub path: Option<PathBuf>,
    pub text: String,
    /// 判定された言語 ID (`rust`, `typescript` など)。判別不能なら `None`。
    pub language: Option<String>,
    pub line_ending: LineEnding,
    pub encoding_is_utf8: bool,
    pub read_only: bool,
    /// バックエンド側の版数。GUI からの編集要求に付けて競合を検出する。
    pub version: u64,
}

use crate::ids::BufferId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    pub const fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

/// 構文ハイライトの 1 スパン。文字オフセットで表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub token: TokenKind,
}

/// テーマの配色キーに対応する意味づけ。
///
/// tree-sitter のハイライト名を直接持ち回すと言語ごとに増え続けるため、
/// テーマ側で色を決められる粒度まで畳んでいる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenKind {
    Keyword,
    KeywordControl,
    Function,
    FunctionMacro,
    Type,
    Constructor,
    Variable,
    Parameter,
    Property,
    Constant,
    String,
    StringEscape,
    Number,
    Boolean,
    Comment,
    CommentDoc,
    Operator,
    Punctuation,
    Namespace,
    Attribute,
    Tag,
    Label,
    Regex,
    Text,
    /// Markdown の見出し。本文と同じ色では埋もれるので専用の種別を持つ。
    Heading,
}

// ---------------------------------------------------------------------------
// ファイルシステム
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// `.gitignore` などで無視対象とされているか。エクスプローラーの淡色表示に使う。
    pub is_ignored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Created,
    Modified,
    Removed,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub path: PathBuf,
    /// `Renamed` のときのみ設定される変更後のパス。
    pub to: Option<PathBuf>,
}

/// 開いているワークスペース (ルートフォルダ) の情報。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub root: PathBuf,
    pub name: String,
    /// git リポジトリのルート。ワークスペースが git 管理下にない場合は `None`。
    pub git_root: Option<PathBuf>,
}

use crate::ids::WorkspaceId;

// ---------------------------------------------------------------------------
// 検索
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub pattern: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub include_ignored: bool,
    /// 打ち切り件数。0 は無制限。
    pub max_results: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            is_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            include_ignored: false,
            max_results: 10_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: PathBuf,
    /// 1 始まりの行番号。ripgrep の出力に合わせる。
    pub line_number: u32,
    pub line_text: String,
    /// `line_text` 内での一致範囲 (文字オフセット)。1 行に複数一致することがある。
    pub matches: Vec<TextRange>,
}

/// ファイルのクイックオープン用の候補。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCandidate {
    pub path: PathBuf,
    /// ワークスペースルートからの相対表示名。
    pub relative: String,
    pub score: i32,
    /// `relative` のうちマッチした文字位置。UI の強調表示に使う。
    pub match_positions: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitStatusCode {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: PathBuf,
    /// リネーム元。`Renamed` のときのみ設定される。
    pub original_path: Option<PathBuf>,
    pub index: GitStatusCode,
    pub worktree: GitStatusCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitRepoStatus {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<GitFileStatus>,
    /// リベース中・マージ中などの進行中操作。
    pub in_progress: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HunkKind {
    Added,
    Modified,
    Removed,
}

/// 行ガターに出すインライン diff の 1 単位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub kind: HunkKind,
    /// 変更後 (作業ツリー) の 0 始まり開始行。
    pub new_start: u32,
    pub new_lines: u32,
    pub old_start: u32,
    pub old_lines: u32,
    /// 削除された行の内容。インライン表示に使う。
    pub removed_text: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    /// UNIX エポック秒。
    pub timestamp: i64,
    pub summary: String,
    /// 0 始まりの行番号。
    pub line: u32,
    /// まだコミットされていない行か。
    pub is_uncommitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBranch {
    pub name: String,
    pub is_head: bool,
    pub is_remote: bool,
    pub upstream: Option<String>,
    pub last_commit_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommitInfo {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    pub email: String,
    pub timestamp: i64,
    pub summary: String,
    pub body: String,
}

// ---------------------------------------------------------------------------
// LSP (Nebula 側の簡約表現)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: SpanRange,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    /// 実際に挿入するテキスト。
    pub insert_text: String,
    /// 挿入時に置換する範囲。`None` なら現在の単語を置換する。
    pub replace_range: Option<SpanRange>,
    pub sort_text: Option<String>,
    pub filter_text: Option<String>,
    /// スニペット構文 (`$1` 等) を含むか。
    pub is_snippet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverInfo {
    /// Markdown 文字列。
    pub contents: String,
    pub range: Option<SpanRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationLink {
    pub path: PathBuf,
    pub range: SpanRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub range: SpanRange,
    /// 名前部分だけの範囲。ジャンプ先に使う。
    pub selection_range: SpanRange,
    pub children: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEditOp {
    pub range: SpanRange,
    pub new_text: String,
}

/// 複数ファイルにまたがる編集 (リネーム等)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    pub changes: Vec<(PathBuf, Vec<TextEditOp>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureHelp {
    pub signatures: Vec<SignatureInfo>,
    pub active_signature: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Vec<String>,
    pub active_parameter: Option<u32>,
}

/// 起動中の言語サーバーの状態。ステータスバー表示に使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspServerStatus {
    pub language: String,
    pub name: String,
    pub state: LspServerState,
    /// 進行中の作業 (indexing 等) の説明。
    pub progress: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LspServerState {
    Starting,
    Running,
    Failed,
    Stopped,
    /// 実行ファイルが見つからず起動できなかった。
    NotInstalled,
}

// ---------------------------------------------------------------------------
// ターミナル
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermColor {
    /// 前景・背景それぞれの既定色。テーマ側で解決する。
    Default,
    /// 256 色パレット。0-15 はテーマの ANSI 16 色に対応させる。
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// セルの装飾。ビットフラグで持つ。
pub mod cell_flags {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const INVERSE: u8 = 1 << 3;
    pub const STRIKETHROUGH: u8 = 1 << 4;
    pub const DIM: u8 = 1 << 5;
    /// 全角文字の後続セル。描画時は読み飛ばす。
    pub const WIDE_TRAILER: u8 = 1 << 6;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCell {
    pub ch: char,
    pub fg: TermColor,
    pub bg: TermColor,
    pub flags: u8,
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: TermColor::Default,
            bg: TermColor::Default,
            flags: 0,
        }
    }
}

/// 差分更新されたターミナルの状態。
///
/// 毎フレーム全画面を送ると `rows * cols` 個のセルが IPC を流れて重いので、
/// 変化した行だけを送る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalUpdate {
    pub id: TerminalId,
    /// (行番号, その行の全セル) の並び。行番号は表示領域の 0 始まり。
    pub dirty_lines: Vec<(u16, Vec<TerminalCell>)>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub title: Option<String>,
    /// スクロールバック総行数。スクロールバー描画に使う。
    pub scrollback_len: usize,
    /// 端末がベルを鳴らしたか。
    pub bell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSpec {
    pub workspace: WorkspaceId,
    /// 起動するシェル。`None` なら既定のシェルをログインシェルとして起動する。
    pub shell: Option<String>,
    /// シェルへ渡す引数。`shell` が `None` のときは使わない
    /// (既定のシェルはログインシェルとして起動するため引数を取らない)。
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// Codex 会話に流れるイベント。
///
/// codex app-server のプロトコルは巨大で版により変わるため、バックエンドで
/// Nebula の語彙へ翻訳する。翻訳できなかったものは `Raw` として通す。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CodexEvent {
    /// 会話が確立した。
    SessionConfigured {
        conversation: CodexConversationId,
        model: String,
        /// 再開されたセッションの場合の元 ID。
        rollout_path: Option<PathBuf>,
    },
    /// アシスタント応答の増分。
    AgentMessageDelta {
        conversation: CodexConversationId,
        delta: String,
    },
    /// アシスタント応答が 1 つ完了した。
    AgentMessage {
        conversation: CodexConversationId,
        text: String,
    },
    /// 推論 (reasoning) の増分。
    ReasoningDelta {
        conversation: CodexConversationId,
        delta: String,
    },
    /// コマンド実行が始まった。
    ExecBegin {
        conversation: CodexConversationId,
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
    },
    /// コマンドの出力増分。
    ExecOutput {
        conversation: CodexConversationId,
        call_id: String,
        chunk: String,
    },
    /// コマンド実行が終わった。
    ExecEnd {
        conversation: CodexConversationId,
        call_id: String,
        exit_code: i32,
    },
    /// ファイル変更が適用された。
    PatchApplied {
        conversation: CodexConversationId,
        files: Vec<PathBuf>,
    },
    /// 承認を求められている。GUI は `Request::CodexRespondApproval` で応答する。
    ApprovalRequested {
        conversation: CodexConversationId,
        request: CodexApprovalRequest,
    },
    /// ターン完了。
    TurnComplete {
        conversation: CodexConversationId,
        token_usage: Option<CodexTokenUsage>,
    },
    /// エラー。
    Error {
        conversation: CodexConversationId,
        message: String,
    },
    /// 未翻訳の生イベント (JSON 文字列)。
    Raw {
        conversation: Option<CodexConversationId>,
        method: String,
        payload: String,
    },
}

use crate::ids::CodexConversationId;
use crate::ids::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexApprovalRequest {
    /// app-server 側のリクエスト ID。応答時にそのまま返す。
    pub request_id: String,
    pub kind: CodexApprovalKind,
    pub summary: String,
    /// コマンド承認なら実行内容、パッチ承認なら unified diff。
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexApprovalKind {
    ExecCommand,
    ApplyPatch,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexApprovalDecision {
    Approve,
    ApproveForSession,
    Deny,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CodexTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Codex の実行モード。GUI から会話開始時に指定する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSessionSpec {
    pub workspace: WorkspaceId,
    pub model: Option<String>,
    pub approval_policy: CodexApprovalPolicy,
    pub sandbox_policy: CodexSandboxPolicy,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexApprovalPolicy {
    Untrusted,
    OnFailure,
    OnRequest,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexSandboxPolicy {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

// ---------------------------------------------------------------------------
// バックエンドの健全性
// ---------------------------------------------------------------------------

/// 実行ファイルの同一性。パス・更新時刻・サイズの組で「同じビルドか」を判定する。
///
/// git のコミットハッシュではなくこの 3 つを使うのは、開発中の典型的な再ビルドが
/// 「HEAD は変えずワーキングツリーだけ変える」形で起きるため。コミットは変わらない
/// ままバイナリだけが更新されるので、コミットハッシュでは検知できない。
/// cargo は実際に内容が変わったときだけ実行ファイルを書き直すので、
/// (path, mtime, size) が一致していれば同じビルドとみなしてよい。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableIdentity {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

impl ExecutableIdentity {
    /// 指定した実行ファイルの現在の同一性情報を読む。
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            mtime: metadata.modified()?,
            size: metadata.len(),
        })
    }
}

/// ハンドシェイクの応答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeInfo {
    pub protocol_version: u32,
    pub backend_version: String,
    pub pid: u32,
    /// このバックエンドを起動している実行ファイルの同一性。GUI は自分がこれから
    /// 起動するはずの実行ファイルと突き合わせ、古いビルドの生き残りを検知する。
    pub executable: ExecutableIdentity,
    /// 外部ツールの検出結果。GUI は機能の出し分けに使う。
    pub tools: DetectedTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DetectedTools {
    pub git: Option<String>,
    pub ripgrep: Option<String>,
    pub codex: Option<String>,
    pub rust_analyzer: Option<String>,
    pub typescript_language_server: Option<String>,
    pub pyright: Option<String>,
    pub bun: Option<String>,
    pub node: Option<String>,
}
