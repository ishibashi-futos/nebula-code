//! バックエンドが保持する状態。
//!
//! バッファやワークスペースの正本はここにあり、GUI 側が持つのは描画用の複製にすぎない。
//! これが「状態管理バックエンドプロセス」の実体。

use crate::codex::CodexService;
use crate::git::GitService;
use crate::lsp::LspService;
use crate::search::SearchService;
use crate::terminal::TerminalService;
use crate::watch::WatchService;
use nebula_core::language::Language;
use nebula_core::{LanguageRegistry, SyntaxTree, TextBuffer};
use nebula_protocol::{
    BufferId, BufferSnapshot, DetectedTools, Event, ProtocolError, WorkspaceId, WorkspaceInfo,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

/// 開いているワークスペース。
pub struct Workspace {
    pub info: WorkspaceInfo,
}

/// 開いているバッファ 1 つぶんの正本。
pub struct BufferEntry {
    pub id: BufferId,
    pub workspace: WorkspaceId,
    pub path: Option<PathBuf>,
    pub buffer: TextBuffer,
    /// 構文解析器。言語が判別できなかった場合は `None`。
    pub syntax: Option<SyntaxTree>,
    pub language: Option<&'static Language>,
    pub read_only: bool,
    /// 言語サーバーへ通知済みの版数。LSP は単調増加する整数を要求する。
    pub lsp_version: i32,
}

impl BufferEntry {
    pub fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            id: self.id,
            path: self.path.clone(),
            text: self.buffer.text(),
            language: self.language.map(|l| l.id.to_string()),
            line_ending: self.buffer.line_ending(),
            encoding_is_utf8: true,
            read_only: self.read_only,
            version: self.buffer.version(),
        }
    }
}

/// プロセス全体で共有される状態。
///
/// 各サービスは独立に内部可変性を持つ。単一の巨大なロックにしないのは、
/// 検索やターミナル出力のような長時間の処理が編集操作を待たせないようにするため。
pub struct BackendState {
    pub tools: DetectedTools,
    pub events: broadcast::Sender<Event>,
    pub languages: Arc<LanguageRegistry>,
    workspaces: Mutex<HashMap<WorkspaceId, Workspace>>,
    buffers: Mutex<HashMap<BufferId, BufferEntry>>,
    pub search: SearchService,
    pub git: GitService,
    pub lsp: LspService,
    pub terminals: TerminalService,
    pub codex: CodexService,
    pub watcher: WatchService,
}

impl BackendState {
    pub fn new(tools: DetectedTools) -> Arc<Self> {
        // 容量を大きめに取るのは、ターミナル出力の連続更新で購読側が
        // 一時的に遅れても Lagged による取りこぼしを起こしにくくするため。
        let (events, _) = broadcast::channel(4096);
        Arc::new(Self {
            languages: Arc::new(LanguageRegistry::new()),
            workspaces: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            search: SearchService::new(events.clone()),
            git: GitService::new(),
            lsp: LspService::new(events.clone(), tools.clone()),
            terminals: TerminalService::new(events.clone()),
            codex: CodexService::new(events.clone(), tools.clone()),
            watcher: WatchService::new(events.clone()),
            tools,
            events,
        })
    }

    /// 全クライアントへイベントを配る。購読者が居なくても失敗扱いにしない。
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn workspaces(&self) -> MutexGuard<'_, HashMap<WorkspaceId, Workspace>> {
        self.workspaces.lock().expect("ワークスペースのロック")
    }

    pub fn buffers(&self) -> MutexGuard<'_, HashMap<BufferId, BufferEntry>> {
        self.buffers.lock().expect("バッファのロック")
    }

    /// ワークスペースのルートを取得する。存在しなければエラー。
    pub fn workspace_root(&self, id: WorkspaceId) -> Result<PathBuf, ProtocolError> {
        self.workspaces()
            .get(&id)
            .map(|w| w.info.root.clone())
            .ok_or_else(|| ProtocolError::not_found(format!("ワークスペース {id} は開かれていません")))
    }

    /// ワークスペースに対応する git リポジトリルート。
    pub fn git_root(&self, id: WorkspaceId) -> Result<PathBuf, ProtocolError> {
        self.workspaces()
            .get(&id)
            .and_then(|w| w.info.git_root.clone())
            .ok_or_else(|| ProtocolError::not_found("git リポジトリではありません"))
    }

    /// バッファに対して閉じた操作を行う。ロックを跨いで `await` しないための入口。
    pub fn with_buffer<T>(
        &self,
        id: BufferId,
        f: impl FnOnce(&mut BufferEntry) -> Result<T, ProtocolError>,
    ) -> Result<T, ProtocolError> {
        let mut buffers = self.buffers();
        let entry = buffers
            .get_mut(&id)
            .ok_or_else(|| ProtocolError::not_found(format!("バッファ {id} は開かれていません")))?;
        f(entry)
    }

    /// 指定パスを開いているバッファを探す。外部変更の反映に使う。
    pub fn buffer_for_path(&self, path: &Path) -> Option<BufferId> {
        self.buffers()
            .values()
            .find(|b| b.path.as_deref() == Some(path))
            .map(|b| b.id)
    }

    /// 全サービスを停止する。
    pub async fn shutdown(&self) {
        self.search.shutdown();
        self.terminals.shutdown();
        self.watcher.shutdown();
        self.lsp.shutdown().await;
        self.codex.shutdown().await;
    }
}
