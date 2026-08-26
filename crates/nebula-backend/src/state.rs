//! バックエンドが保持する状態。
//!
//! バッファやワークスペースの正本はここにあり、GUI 側が持つのは描画用の複製にすぎない。
//! これが「状態管理バックエンドプロセス」の実体。

use crate::codex::CodexService;
use crate::git::GitService;
use crate::lsp::LspService;
use crate::search::SearchService;
use crate::terminal::TerminalService;
use crate::tools::SharedTools;
use crate::watch::WatchService;
use nebula_core::language::Language;
use nebula_core::{LanguageRegistry, SyntaxTree, TextBuffer};
use nebula_protocol::{
    BufferId, BufferSnapshot, DetectedTools, Event, ExecutableIdentity, ProtocolError, WorkspaceId,
    WorkspaceInfo,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
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
    /// 検出済みの外部ツール。構築時点では空のことがあり、検出完了時に
    /// `apply_detected_tools` で差し替わる。`LspService`・`CodexService` も
    /// この `Arc` を複製して持つので、差し替えは 1 箇所で全員に伝わる。
    pub tools: SharedTools,
    /// このプロセス自身の実行ファイルの同一性。起動時に 1 度だけ読み、以後は
    /// 固定する。ハンドシェイクのたびに読み直すと、動作中に上書きされた
    /// (再ビルドされた) 実行ファイルの新しい mtime/size を返してしまい、
    /// 「古いバックエンドの生き残り」を検知できなくなる。
    pub executable: ExecutableIdentity,
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
        // current_exe() の失敗は「自分が何者か分からない」という致命的な状況で、
        // 実行ファイルが動作中に削除されたなど極めて例外的な場合しか起きない。
        // 中途半端な既定値でごまかさず、ここで止める。
        let executable = std::env::current_exe()
            .and_then(|path| ExecutableIdentity::from_path(&path))
            .expect("自分の実行ファイルの情報を読めません");
        let tools: SharedTools = Arc::new(RwLock::new(tools));
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
            executable,
            tools,
            events,
        })
    }

    /// 全クライアントへイベントを配る。購読者が居なくても失敗扱いにしない。
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// 非同期に完了した検出結果を反映し、GUI へ通知する。
    pub fn apply_detected_tools(&self, tools: DetectedTools) {
        *self.tools.write().expect("検出結果のロック") = tools.clone();
        self.emit(Event::ToolsDetected { tools });
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
            .ok_or_else(|| {
                ProtocolError::not_found(format!("ワークスペース {id} は開かれていません"))
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 「先にソケットを開き、検出は非同期で行って結果をイベントで通知する」という
    /// 設計の要。検出はバックエンド起動後にしか終わらないので、共有状態への反映と
    /// GUI へのイベント通知の両方が確実に起きることをここで保証する。
    #[test]
    fn 検出結果の適用でイベントが流れて共有状態も変わる() {
        let state = BackendState::new(DetectedTools::default());
        let mut events = state.events.subscribe();

        let detected = DetectedTools {
            git: Some("git version 2.43.0".to_string()),
            ..DetectedTools::default()
        };
        state.apply_detected_tools(detected.clone());

        assert_eq!(*state.tools.read().expect("検出結果のロック"), detected);
        match events.try_recv() {
            Ok(Event::ToolsDetected { tools }) => assert_eq!(tools, detected),
            other => panic!("Event::ToolsDetected が届かない: {other:?}"),
        }
    }
}
