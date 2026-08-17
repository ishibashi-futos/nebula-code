//! LSP 連携。JSON-RPC over stdio の言語サーバークライアント。
//!
//! 責務を 4 つに割っている。
//!
//! - [`framing`] … `Content-Length` ヘッダによるフレーミング (nebula-protocol とは別物)。
//! - [`client`]  … プロセス起動と要求 ID の待ち合わせ。
//! - [`offset`]  … LSP の UTF-16 桁と Nebula の char 桁の相互変換。
//! - [`convert`] … lsp-types から nebula-protocol への翻訳。
//!
//! このモジュール本体はサーバーの生存管理と文書同期だけを扱う。
//!
//! 言語サーバーは「あれば使う」。実行ファイルが無くても編集は続けられるべきなので、
//! 起動できなかった場合は状態を [`LspServerState::NotInstalled`] にするだけで
//! エラーにはしない。同じ理由で、サーバーが居ないときの問い合わせは
//! 空の結果を返す (要求のたびにエラーを出すと GUI が通知で埋まる)。

mod client;
mod convert;
mod framing;
mod offset;
mod uri;

use client::{Client, Incoming};
use lsp_types as lsp;
use nebula_protocol::{
    CodeAction, CompletionItem, DetectedTools, Event, HoverInfo, LocationLink, LspServerState,
    LspServerStatus, Position, ProtocolError, ProtocolErrorKind, SignatureHelp, SpanRange,
    SymbolInfo, TextEditOp, WorkspaceEdit, WorkspaceId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedReceiver;

/// 通常の要求のタイムアウト。
///
/// 応答が返らない要求で GUI が固まらないよう、すべての要求に必ず付ける。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// initialize は索引付けの準備を含むぶん長くかかる。
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);

/// 終了要求はすぐ返るはずなので短くする。応じなければ強制終了する。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// 言語サーバーの対応表
// ---------------------------------------------------------------------------

/// 起動する言語サーバーの定義。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerSpec {
    /// 1 プロセスを共有する単位の名前。
    ///
    /// TypeScript / TSX / JavaScript は同じ `typescript-language-server` が面倒を見るので、
    /// 言語 ID ごとに別プロセスを立てず 1 つにまとめる。
    family: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

fn server_spec(language: &str) -> Option<ServerSpec> {
    match language {
        "rust" => Some(ServerSpec {
            family: "rust",
            program: "rust-analyzer",
            args: &[],
        }),
        "typescript" | "tsx" | "javascript" => Some(ServerSpec {
            family: "typescript",
            program: "typescript-language-server",
            args: &["--stdio"],
        }),
        "python" => Some(ServerSpec {
            family: "python",
            program: "pyright-langserver",
            args: &["--stdio"],
        }),
        _ => None,
    }
}

/// サーバー 1 プロセスを一意に決める鍵。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServerKey {
    root: PathBuf,
    family: &'static str,
}

// ---------------------------------------------------------------------------
// サービス本体
// ---------------------------------------------------------------------------

pub struct LspService {
    inner: Arc<Inner>,
    /// 起動処理を直列化する。
    ///
    /// 複数のバッファを同時に開くと `ensure_server` が並行で走る。この関門が無いと
    /// 同じ言語サーバーを何プロセスも立ち上げてしまう。
    startup: tokio::sync::Mutex<()>,
    /// 起動時に検出したツール。
    ///
    /// 未インストール判定にはこれを使わず、実際の spawn 失敗を見る。検出は起動時の
    /// 1 回きりなので、後から入れた言語サーバーを永久に無いものと扱ってしまうため。
    #[allow(dead_code)]
    tools: DetectedTools,
}

/// 非同期タスクと共有する状態。
///
/// 受信ポンプ (サーバーからの通知を処理するタスク) が [`LspService`] 本体ではなく
/// これを保持するのは、`LspService` が [`BackendState`] のフィールドとして置かれ、
/// 単体では `Arc` にできないため。
///
/// [`BackendState`]: crate::state::BackendState
struct Inner {
    events: broadcast::Sender<Event>,
    servers: Mutex<HashMap<ServerKey, Arc<Server>>>,
    documents: Mutex<HashMap<PathBuf, Document>>,
    statuses: Mutex<HashMap<ServerKey, LspServerStatus>>,
    /// 直前の `textDocument/codeAction` の結果。`apply_code_action` の索引が指す先。
    code_actions: Mutex<HashMap<PathBuf, Vec<lsp::CodeActionOrCommand>>>,
}

struct Server {
    key: ServerKey,
    spec: ServerSpec,
    client: Arc<Client>,
    /// フルテキストの `didChange` を受け付けるか。initialize の応答から判定する。
    full_sync: bool,
}

/// サーバーへ同期済みの文書。
struct Document {
    key: ServerKey,
    /// LSP へ通知した版数。単調増加していることだけが要件。
    version: i32,
    text: String,
}

/// 文書同期で送るべき通知。
enum DocumentSync {
    Open,
    Change(i32),
    None,
}

impl LspService {
    pub fn new(events: broadcast::Sender<Event>, tools: DetectedTools) -> Self {
        Self {
            inner: Arc::new(Inner {
                events,
                servers: Mutex::new(HashMap::new()),
                documents: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                code_actions: Mutex::new(HashMap::new()),
            }),
            startup: tokio::sync::Mutex::new(()),
            tools,
        }
    }

    // -- サーバー管理 --

    /// 対応する言語サーバーを起動する。既に動いていれば何もしない。
    pub async fn ensure_server(&self, root: &Path, language: &str) -> Result<(), ProtocolError> {
        let Some(spec) = server_spec(language) else {
            // 対応表に無い言語は構文着色だけで使う。
            return Ok(());
        };
        let key = ServerKey {
            root: root.to_path_buf(),
            family: spec.family,
        };
        if self.inner.server(&key).is_some() {
            return Ok(());
        }

        let _guard = self.startup.lock().await;
        // 関門を待っている間に別の呼び出しが起動を終えていることがある。
        if self.inner.server(&key).is_some() {
            return Ok(());
        }

        self.inner
            .set_status(&key, spec, LspServerState::Starting, None);
        match start_server(key.clone(), spec, self.inner.clone()).await {
            Ok(server) => {
                self.inner
                    .servers
                    .lock()
                    .expect("サーバー表のロック")
                    .insert(key.clone(), Arc::new(server));
                self.inner
                    .set_status(&key, spec, LspServerState::Running, None);
                Ok(())
            }
            Err(error) if error.kind == ProtocolErrorKind::NotFound => {
                self.inner
                    .set_status(&key, spec, LspServerState::NotInstalled, None);
                Ok(())
            }
            Err(error) => {
                self.inner.set_status(
                    &key,
                    spec,
                    LspServerState::Failed,
                    Some(error.message.clone()),
                );
                Err(error)
            }
        }
    }

    pub fn statuses(&self) -> Vec<LspServerStatus> {
        let mut statuses: Vec<_> = self
            .inner
            .statuses
            .lock()
            .expect("状態表のロック")
            .values()
            .cloned()
            .collect();
        statuses.sort_by(|a, b| a.language.cmp(&b.language));
        statuses
    }

    /// サーバーを停止して起動し直す。設定変更や索引の壊れからの復帰に使う。
    pub async fn restart(&self, root: &Path, language: &str) -> Result<(), ProtocolError> {
        let Some(spec) = server_spec(language) else {
            return Err(ProtocolError::unsupported(format!(
                "{language} に対応する言語サーバーはありません"
            )));
        };
        let key = ServerKey {
            root: root.to_path_buf(),
            family: spec.family,
        };
        self.inner.stop(&key, spec).await;
        self.ensure_server(root, language).await
    }

    pub async fn shutdown(&self) {
        let servers: Vec<Arc<Server>> = {
            let mut map = self.inner.servers.lock().expect("サーバー表のロック");
            map.drain().map(|(_, server)| server).collect()
        };
        for server in servers {
            server.stop().await;
            self.inner
                .set_status(&server.key, server.spec, LspServerState::Stopped, None);
        }
        self.inner.documents().clear();
        // 一覧した時点のコードアクションも捨てる。残すと、停止後に
        // `apply_code_action` が古い応答から編集内容を作って返してしまう。
        self.inner
            .code_actions
            .lock()
            .expect("コードアクション表のロック")
            .clear();
    }

    // -- 文書同期 --

    /// 文書を開いたことをサーバーへ知らせる。
    ///
    /// `language` と `version` は使わない。言語はパスから判定した方が要求経路
    /// (ホバー等は言語を受け取らない) と食い違わず、版数はこのモジュール内で
    /// 単調増加させた方が確実なため。
    pub async fn did_open(&self, path: &Path, _language: &str, _version: i32, text: &str) {
        self.sync_document(path, text).await;
    }

    /// 編集をサーバーへ知らせる。**フルテキスト同期**で送る。
    ///
    /// 差分同期にすると、GUI から届く編集列と LSP の範囲表現 (UTF-16 桁) を
    /// 突き合わせる処理が要る。得られるのは大きなファイルでの通信量削減だけで、
    /// ずれたときの壊れ方が深刻なので採らない。
    pub async fn did_change(&self, path: &Path, _language: &str, _version: i32, text: &str) {
        self.sync_document(path, text).await;
    }

    pub async fn did_save(&self, path: &Path, text: &str) {
        let Some(server) = self.sync_document(path, text).await else {
            return;
        };
        let Ok(uri) = uri::path_to_uri(path) else {
            return;
        };
        server.client.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri }, "text": text }),
        );
    }

    pub async fn did_close(&self, path: &Path) {
        let document = self.inner.documents().remove(path);
        self.inner
            .code_actions
            .lock()
            .expect("コードアクション表のロック")
            .remove(path);
        let Some(document) = document else {
            return;
        };
        let (Some(server), Ok(uri)) = (self.inner.server(&document.key), uri::path_to_uri(path))
        else {
            return;
        };
        server
            .client
            .notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
    }

    /// 要求前に、サーバーが持つ文書の内容を `text` と一致させる。
    ///
    /// GUI からの要求には必ず最新本文が付いてくるので、それを使って
    /// `didOpen` / `didChange` を補う。`did_open` を経由していない経路
    /// (サーバー再起動直後など) でも位置がずれない。
    async fn sync_document(&self, path: &Path, text: &str) -> Option<Arc<Server>> {
        let (server, language_id) = self.resolve_server(path)?;
        let action = {
            let mut documents = self.inner.documents();
            match documents.get_mut(path) {
                Some(document) if document.key == server.key => {
                    if document.text == text {
                        DocumentSync::None
                    } else {
                        document.version += 1;
                        document.text = text.to_string();
                        DocumentSync::Change(document.version)
                    }
                }
                // 未登録、またはサーバーが入れ替わった場合は開き直す。
                _ => {
                    documents.insert(
                        path.to_path_buf(),
                        Document {
                            key: server.key.clone(),
                            version: 1,
                            text: text.to_string(),
                        },
                    );
                    DocumentSync::Open
                }
            }
        };

        let uri = uri::path_to_uri(path).ok()?;
        match action {
            DocumentSync::Open => server.client.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }),
            ),
            DocumentSync::Change(version) if server.full_sync => server.client.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    // `range` の無い変更イベントは全文置換を意味する。
                    // 差分同期しか宣言していないサーバーもこの形は受け付ける。
                    "contentChanges": [{ "text": text }],
                }),
            ),
            DocumentSync::Change(_) | DocumentSync::None => {}
        }
        Some(server)
    }

    /// パスから担当サーバーを引く。言語 ID も返す。
    fn resolve_server(&self, path: &Path) -> Option<(Arc<Server>, String)> {
        let language = nebula_core::language::detect_language(path)?;
        let spec = server_spec(language.id)?;
        let servers = self.inner.servers.lock().expect("サーバー表のロック");
        servers
            .values()
            // ルートが入れ子になっている場合は、より深いワークスペースの担当とする。
            .filter(|server| server.key.family == spec.family && path.starts_with(&server.key.root))
            .max_by_key(|server| server.key.root.as_os_str().len())
            .cloned()
            .map(|server| (server, language.id.to_string()))
    }

    // -- 要求 --

    pub async fn hover(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<Option<HoverInfo>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(None);
        };
        let value = server
            .client
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let hover: Option<lsp::Hover> = parse(value)?;
        Ok(hover.map(|hover| convert::hover(hover, text)))
    }

    pub async fn completion(
        &self,
        path: &Path,
        text: &str,
        position: Position,
        trigger: Option<&str>,
    ) -> Result<Vec<CompletionItem>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(Vec::new());
        };
        // triggerKind: 1 = 明示的な呼び出し、2 = 文字入力による誘発。
        let context = match trigger {
            Some(character) => json!({ "triggerKind": 2, "triggerCharacter": character }),
            None => json!({ "triggerKind": 1 }),
        };
        let value = server
            .client
            .request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                    "context": context,
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let response: Option<lsp::CompletionResponse> = parse(value)?;
        Ok(response
            .map(|response| convert::completions(response, text))
            .unwrap_or_default())
    }

    pub async fn definition(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<Vec<LocationLink>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(Vec::new());
        };
        let value = server
            .client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let response: Option<lsp::GotoDefinitionResponse> = parse(value)?;
        let flat = response.map(convert::flatten_definition).unwrap_or_default();
        Ok(self.to_location_links(flat).await)
    }

    pub async fn references(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<Vec<LocationLink>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(Vec::new());
        };
        let value = server
            .client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                    "context": { "includeDeclaration": true },
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let locations: Option<Vec<lsp::Location>> = parse(value)?;
        let flat = locations.map(convert::flatten_locations).unwrap_or_default();
        Ok(self.to_location_links(flat).await)
    }

    pub async fn document_symbols(&self, path: &Path) -> Result<Vec<SymbolInfo>, ProtocolError> {
        let Some(text) = self.text_of(path).await else {
            return Ok(Vec::new());
        };
        let Some(server) = self.sync_document(path, &text).await else {
            return Ok(Vec::new());
        };
        let value = server
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri::path_to_uri(path)? } }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let response: Option<lsp::DocumentSymbolResponse> = parse(value)?;
        Ok(response
            .map(|response| convert::document_symbols(response, &text))
            .unwrap_or_default())
    }

    pub async fn format(&self, path: &Path, text: &str) -> Result<Vec<TextEditOp>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(Vec::new());
        };
        // 整形の刻み幅は言語定義から採る。サーバー既定に任せると Nebula の
        // インデント設定と食い違った結果が返る。
        let language = nebula_core::language::detect_language(path);
        let value = server
            .client
            .request(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "options": {
                        "tabSize": language.map(|l| l.indent_width).unwrap_or(4),
                        "insertSpaces": !language.map(|l| l.use_tabs).unwrap_or(false),
                    },
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let edits: Option<Vec<lsp::TextEdit>> = parse(value)?;
        Ok(convert::text_edits(edits.unwrap_or_default(), text))
    }

    pub async fn rename(
        &self,
        path: &Path,
        text: &str,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(WorkspaceEdit {
                changes: Vec::new(),
            });
        };
        let value = server
            .client
            .request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                    "newName": new_name,
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let edit: Option<lsp::WorkspaceEdit> = parse(value)?;
        match edit {
            Some(edit) => Ok(self.to_workspace_edit(edit).await),
            None => Err(ProtocolError::external(
                "この位置の名前は変更できません",
            )),
        }
    }

    pub async fn signature_help(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<Option<SignatureHelp>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(None);
        };
        let value = server
            .client
            .request(
                "textDocument/signatureHelp",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "position": offset::to_lsp_position(text, position),
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let help: Option<lsp::SignatureHelp> = parse(value)?;
        Ok(help.map(convert::signature_help))
    }

    pub async fn code_actions(
        &self,
        path: &Path,
        text: &str,
        range: SpanRange,
    ) -> Result<Vec<CodeAction>, ProtocolError> {
        let Some(server) = self.sync_document(path, text).await else {
            return Ok(Vec::new());
        };
        let value = server
            .client
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri::path_to_uri(path)? },
                    "range": offset::to_lsp_range(text, range),
                    "context": { "diagnostics": [] },
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        let actions: Option<lsp::CodeActionResponse> = parse(value)?;
        let actions = actions.unwrap_or_default();
        let converted = convert::code_actions(&actions);
        // 適用は索引で指定されるので、元の応答を覚えておく必要がある。
        self.inner
            .code_actions
            .lock()
            .expect("コードアクション表のロック")
            .insert(path.to_path_buf(), actions);
        Ok(converted)
    }

    /// 直前の [`Self::code_actions`] の `index` 番目を編集内容に解決する。
    pub async fn apply_code_action(
        &self,
        path: &Path,
        index: usize,
    ) -> Result<WorkspaceEdit, ProtocolError> {
        let action = self
            .inner
            .code_actions
            .lock()
            .expect("コードアクション表のロック")
            .get(path)
            .and_then(|actions| actions.get(index).cloned())
            .ok_or_else(|| {
                ProtocolError::invalid(format!("{index} 番目のコードアクションはありません"))
            })?;

        let action = match action {
            lsp::CodeActionOrCommand::CodeAction(action) => action,
            // コマンド形式は `workspace/executeCommand` の実行が要り、結果は
            // サーバーからの `workspace/applyEdit` として返る。編集内容を
            // 同期的に返せないので、この経路は対応しない。
            lsp::CodeActionOrCommand::Command(command) => {
                return Err(ProtocolError::unsupported(format!(
                    "コマンド形式のコードアクションは適用できません: {}",
                    command.title
                )));
            }
        };

        let edit = match action.edit {
            Some(edit) => edit,
            // 編集内容を遅延生成するサーバーがある。解決要求で取り直す。
            None => {
                let Some(server) = self.resolve_server(path).map(|(server, _)| server) else {
                    return Err(ProtocolError::external(
                        "言語サーバーが動いていません",
                    ));
                };
                let value = server
                    .client
                    .request(
                        "codeAction/resolve",
                        serde_json::to_value(&action).map_err(|e| {
                            ProtocolError::internal(format!("解決要求を組み立てられません: {e}"))
                        })?,
                        REQUEST_TIMEOUT,
                    )
                    .await?;
                let resolved: lsp::CodeAction = parse(value)?;
                resolved.edit.ok_or_else(|| {
                    ProtocolError::external(format!(
                        "「{}」は編集内容を返しませんでした",
                        action.title
                    ))
                })?
            }
        };
        Ok(self.to_workspace_edit(edit).await)
    }

    // -- 結果の後処理 --

    /// 他ファイルを指す結果の桁を char 単位に直す。
    async fn to_location_links(&self, flat: Vec<(PathBuf, lsp::Range)>) -> Vec<LocationLink> {
        let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut links = Vec::with_capacity(flat.len());
        for (path, range) in flat {
            let text = self.cached_text(&mut texts, &path).await;
            links.push(LocationLink {
                range: span_from(text, range),
                path,
            });
        }
        links
    }

    async fn to_workspace_edit(&self, edit: lsp::WorkspaceEdit) -> WorkspaceEdit {
        let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut changes = Vec::new();
        for (path, edits) in convert::flatten_workspace_edit(edit) {
            let text = self.cached_text(&mut texts, &path).await.cloned();
            let operations = edits
                .into_iter()
                .map(|edit| TextEditOp {
                    range: span_from(text.as_ref(), edit.range),
                    new_text: edit.new_text,
                })
                .collect();
            changes.push((path, operations));
        }
        WorkspaceEdit { changes }
    }

    /// 1 回の変換処理の中で同じファイルを何度も読まないようにする。
    async fn cached_text<'a>(
        &self,
        cache: &'a mut HashMap<PathBuf, Option<String>>,
        path: &Path,
    ) -> Option<&'a String> {
        if !cache.contains_key(path) {
            cache.insert(path.to_path_buf(), self.text_of(path).await);
        }
        cache.get(path).and_then(|text| text.as_ref())
    }

    /// 桁変換に使う本文。開いていればその内容、なければディスクから読む。
    async fn text_of(&self, path: &Path) -> Option<String> {
        if let Some(text) = self.inner.documents().get(path).map(|d| d.text.clone()) {
            return Some(text);
        }
        tokio::fs::read_to_string(path).await.ok()
    }
}

/// UTF-16 桁を char 桁に直す。本文が読めない場合は行番号だけを信じる。
fn span_from(text: Option<&String>, range: lsp::Range) -> SpanRange {
    match text {
        Some(text) => offset::from_lsp_range(text, range),
        // ASCII のみの行なら UTF-16 桁と char 桁は一致する。少なくとも行は合う。
        None => SpanRange::new(
            Position::new(range.start.line, range.start.character),
            Position::new(range.end.line, range.end.character),
        ),
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value)
        .map_err(|e| ProtocolError::external(format!("言語サーバーの応答を解釈できません: {e}")))
}

impl Server {
    /// 正規の手順で終了させる。応じなければ強制終了する。
    async fn stop(&self) {
        let _ = self
            .client
            .request("shutdown", Value::Null, SHUTDOWN_TIMEOUT)
            .await;
        self.client.notify("exit", Value::Null);
        self.client.kill();
    }
}

// ---------------------------------------------------------------------------
// 起動
// ---------------------------------------------------------------------------

async fn start_server(
    key: ServerKey,
    spec: ServerSpec,
    inner: Arc<Inner>,
) -> Result<Server, ProtocolError> {
    let (client, incoming) = Client::spawn(spec.program, spec.args, &key.root)?;
    let client = Arc::new(client);

    // 受信ポンプは initialize より先に回す。応答を待っている間にも
    // 進捗通知やログが届くため。
    tokio::spawn(pump(
        inner.clone(),
        client.clone(),
        key.clone(),
        spec,
        incoming,
    ));

    match handshake(&client, &key, spec, &inner).await {
        Ok(full_sync) => Ok(Server {
            key,
            spec,
            client,
            full_sync,
        }),
        // 握手に失敗したら必ず止める。ポンプが `Arc<Client>` を持ち続けるので、
        // ここで明示的に殺さないと子プロセスが残る。
        Err(error) => {
            client.kill();
            Err(error)
        }
    }
}

/// initialize → initialized を通し、フルテキスト同期が使えるかを返す。
async fn handshake(
    client: &Client,
    key: &ServerKey,
    spec: ServerSpec,
    inner: &Inner,
) -> Result<bool, ProtocolError> {
    let root_uri = uri::path_to_uri(&key.root)?;
    let params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": client_capabilities(),
        "workspaceFolders": [{
            "uri": root_uri,
            "name": key.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        }],
        "clientInfo": { "name": "Nebula Code", "version": env!("CARGO_PKG_VERSION") },
    });
    let value = client
        .request("initialize", params, INITIALIZE_TIMEOUT)
        .await?;
    let result: lsp::InitializeResult = parse(value)?;
    client.notify("initialized", json!({}));

    // 要件どおりフルテキスト同期で送るので、サーバーが受け付けるか確かめる。
    let full_sync = accepts_full_sync(result.capabilities.text_document_sync.as_ref());
    if !full_sync {
        inner.emit(Event::LspMessage {
            language: spec.family.to_string(),
            message: format!(
                "{} は文書変更の通知を受け付けません。編集内容は反映されません。",
                spec.program
            ),
        });
    }
    Ok(full_sync)
}

/// 全文置換の `didChange` を送ってよいか判定する。
///
/// 差分同期 (`INCREMENTAL`) を宣言しているサーバーでも、範囲を持たない変更イベントは
/// 全文置換として受け付ける仕様。よって拒むのは「変更通知そのものが不要」な場合だけ。
fn accepts_full_sync(sync: Option<&lsp::TextDocumentSyncCapability>) -> bool {
    match sync {
        Some(lsp::TextDocumentSyncCapability::Kind(kind)) => *kind != lsp::TextDocumentSyncKind::NONE,
        Some(lsp::TextDocumentSyncCapability::Options(options)) => options
            .change
            .is_some_and(|kind| kind != lsp::TextDocumentSyncKind::NONE),
        None => false,
    }
}

fn client_capabilities() -> lsp::ClientCapabilities {
    let markup = Some(vec![lsp::MarkupKind::Markdown, lsp::MarkupKind::PlainText]);
    lsp::ClientCapabilities {
        workspace: Some(lsp::WorkspaceClientCapabilities {
            workspace_edit: Some(lsp::WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                ..Default::default()
            }),
            configuration: Some(true),
            ..Default::default()
        }),
        text_document: Some(lsp::TextDocumentClientCapabilities {
            synchronization: Some(lsp::TextDocumentSyncClientCapabilities {
                did_save: Some(true),
                ..Default::default()
            }),
            hover: Some(lsp::HoverClientCapabilities {
                content_format: markup.clone(),
                ..Default::default()
            }),
            completion: Some(lsp::CompletionClientCapabilities {
                completion_item: Some(lsp::CompletionItemCapability {
                    snippet_support: Some(true),
                    documentation_format: markup.clone(),
                    insert_replace_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            definition: Some(lsp::GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            references: Some(lsp::ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            document_symbol: Some(lsp::DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            formatting: Some(lsp::DocumentFormattingClientCapabilities {
                dynamic_registration: Some(false),
            }),
            rename: Some(lsp::RenameClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            signature_help: Some(lsp::SignatureHelpClientCapabilities {
                signature_information: Some(lsp::SignatureInformationSettings {
                    documentation_format: markup,
                    parameter_information: Some(lsp::ParameterInformationSettings {
                        label_offset_support: Some(true),
                    }),
                    active_parameter_support: Some(true),
                }),
                ..Default::default()
            }),
            code_action: Some(lsp::CodeActionClientCapabilities {
                code_action_literal_support: Some(lsp::CodeActionLiteralSupport {
                    code_action_kind: lsp::CodeActionKindLiteralSupport {
                        value_set: vec![
                            "quickfix".into(),
                            "refactor".into(),
                            "refactor.extract".into(),
                            "refactor.inline".into(),
                            "refactor.rewrite".into(),
                            "source".into(),
                            "source.organizeImports".into(),
                        ],
                    },
                }),
                is_preferred_support: Some(true),
                data_support: Some(true),
                // 編集内容を後から取りに行けることを伝える。
                // これが無いと重いコードアクションを列挙しないサーバーがある。
                resolve_support: Some(lsp::CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".into()],
                }),
                ..Default::default()
            }),
            publish_diagnostics: Some(lsp::PublishDiagnosticsClientCapabilities::default()),
            ..Default::default()
        }),
        // 進捗通知 ($/progress) はこれを宣言しないと送られてこない。索引付けの表示に要る。
        window: Some(lsp::WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// サーバーからの通知・要求
// ---------------------------------------------------------------------------

async fn pump(
    inner: Arc<Inner>,
    client: Arc<Client>,
    key: ServerKey,
    spec: ServerSpec,
    mut incoming: UnboundedReceiver<Incoming>,
) {
    while let Some(message) = incoming.recv().await {
        inner.handle(&client, &key, spec, message).await;
    }
    // 受信経路が閉じた = プロセスが終わった。こちらから停止した場合は
    // 既に表から外れているので、残っているなら予期しない死。
    let removed = inner
        .servers
        .lock()
        .expect("サーバー表のロック")
        .remove(&key)
        .is_some();
    if removed {
        // 文書の同期状態も捨てる。これを残すと、次に同じ鍵で起動した新しい
        // プロセスに対して `sync_document` が「同期済み」と誤判定し、
        // didOpen を送らないまま didChange や要求を投げることになる。
        inner.documents().retain(|_, document| document.key != key);
        inner.set_status(
            &key,
            spec,
            LspServerState::Failed,
            Some("プロセスが終了しました".to_string()),
        );
    }
}

impl Inner {
    fn server(&self, key: &ServerKey) -> Option<Arc<Server>> {
        self.servers
            .lock()
            .expect("サーバー表のロック")
            .get(key)
            .cloned()
    }

    fn documents(&self) -> MutexGuard<'_, HashMap<PathBuf, Document>> {
        self.documents.lock().expect("文書表のロック")
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// 状態を記録して GUI へ通知する。
    ///
    /// `workspace` には [`WorkspaceId`] が要るが、このサービスはルートのパスしか
    /// 受け取らない (`ensure_server(root, language)`)。対応表を持てないので
    /// 「不明」を意味する 0 を入れる。GUI は `language` で識別する。
    fn set_status(
        &self,
        key: &ServerKey,
        spec: ServerSpec,
        state: LspServerState,
        progress: Option<String>,
    ) {
        let status = LspServerStatus {
            language: key.family.to_string(),
            name: spec.program.to_string(),
            state,
            progress,
        };
        self.statuses
            .lock()
            .expect("状態表のロック")
            .insert(key.clone(), status.clone());
        self.emit(Event::LspStatusChanged {
            workspace: WorkspaceId(0),
            status,
        });
    }

    /// 進捗だけを差し替える。状態 (Running 等) は保つ。
    fn set_progress(&self, key: &ServerKey, spec: ServerSpec, progress: Option<String>) {
        let state = self
            .statuses
            .lock()
            .expect("状態表のロック")
            .get(key)
            .map(|status| status.state)
            .unwrap_or(LspServerState::Running);
        self.set_status(key, spec, state, progress);
    }

    async fn stop(&self, key: &ServerKey, spec: ServerSpec) {
        let server = self
            .servers
            .lock()
            .expect("サーバー表のロック")
            .remove(key);
        // 文書の同期状態も捨てる。起動し直したサーバーは何も開いていないため。
        self.documents().retain(|_, document| document.key != *key);
        if let Some(server) = server {
            server.stop().await;
        }
        self.set_status(key, spec, LspServerState::Stopped, None);
    }

    async fn handle(&self, client: &Client, key: &ServerKey, spec: ServerSpec, message: Incoming) {
        let Incoming { method, id, params } = message;
        // サーバー発の要求には必ず応答する。放置すると待ち続けて止まるサーバーがある。
        if let Some(id) = id {
            client.respond(id, empty_result(&method, &params));
        }
        match method.as_str() {
            "textDocument/publishDiagnostics" => self.publish_diagnostics(params).await,
            "window/logMessage" | "window/showMessage" => {
                if let Some(text) = params.get("message").and_then(Value::as_str) {
                    self.emit(Event::LspMessage {
                        language: key.family.to_string(),
                        message: text.to_string(),
                    });
                }
            }
            "$/progress" => {
                if let Ok(params) = serde_json::from_value::<lsp::ProgressParams>(params) {
                    self.set_progress(key, spec, progress_text(params.value));
                }
            }
            _ => {}
        }
    }

    async fn publish_diagnostics(&self, params: Value) {
        let Ok(params) = serde_json::from_value::<lsp::PublishDiagnosticsParams>(params) else {
            return;
        };
        let Some(path) = uri::uri_to_path(&params.uri) else {
            return;
        };
        // 桁を char 単位に直すには本文が要る。開いていなければディスクから読む。
        let opened = self.documents().get(&path).map(|d| d.text.clone());
        let text = match opened {
            Some(text) => text,
            None => match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                // 本文が無いと桁を変換できない。行だけ合った診断は誤解を招くので出さない。
                Err(_) => return,
            },
        };
        self.emit(Event::Diagnostics {
            path,
            diagnostics: convert::diagnostics(params.diagnostics, &text),
        });
    }
}

/// 進捗通知を 1 行の表示文字列にする。終了なら `None`。
fn progress_text(value: lsp::ProgressParamsValue) -> Option<String> {
    let lsp::ProgressParamsValue::WorkDone(work) = value;
    match work {
        lsp::WorkDoneProgress::Begin(begin) => {
            join_progress(&begin.title, begin.message.as_deref(), begin.percentage)
        }
        lsp::WorkDoneProgress::Report(report) => {
            join_progress("", report.message.as_deref(), report.percentage)
        }
        lsp::WorkDoneProgress::End(_) => None,
    }
}

fn join_progress(title: &str, message: Option<&str>, percentage: Option<u32>) -> Option<String> {
    let mut text = title.to_string();
    if let Some(message) = message {
        if !text.is_empty() {
            text.push_str(": ");
        }
        text.push_str(message);
    }
    if let Some(percentage) = percentage {
        text.push_str(&format!(" ({percentage}%)"));
    }
    // 表題も本文も無く割合だけが来る報告があり、そのままだと先頭に空白が残る。
    let text = text.trim();
    // 中身の無い進捗報告はステータスバーに出しても意味がない。
    (!text.is_empty()).then(|| text.to_string())
}

/// サーバー発の要求に返す空の成功応答。
///
/// 内容は処理しないが、形だけは相手が期待する型に合わせる。null を返すと
/// 解釈に失敗して止まる要求があるため。
fn empty_result(method: &str, params: &Value) -> Value {
    match method {
        // 設定は何も持たない。要求された項目数と同じ長さの配列を返す決まり。
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(1);
            Value::Array(vec![Value::Null; count])
        }
        // 編集の自動適用には対応しない。
        "workspace/applyEdit" => json!({ "applied": false }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 言語_id_から起動コマンドを引ける() {
        assert_eq!(server_spec("rust").unwrap().program, "rust-analyzer");
        assert_eq!(
            server_spec("python").unwrap().program,
            "pyright-langserver"
        );
        assert!(server_spec("markdown").is_none());
    }

    #[test]
    fn typescript_系は_1_プロセスを共有する() {
        let family = |id| server_spec(id).unwrap().family;
        assert_eq!(family("typescript"), family("tsx"));
        assert_eq!(family("typescript"), family("javascript"));
        assert_ne!(family("typescript"), family("rust"));
    }

    #[test]
    fn 変更通知が不要なサーバーにはフル同期を送らない() {
        use lsp::TextDocumentSyncCapability as Cap;
        use lsp::TextDocumentSyncKind as Kind;
        assert!(accepts_full_sync(Some(&Cap::Kind(Kind::FULL))));
        // 差分同期を宣言していても、範囲なしの変更イベントは受け付ける。
        assert!(accepts_full_sync(Some(&Cap::Kind(Kind::INCREMENTAL))));
        assert!(!accepts_full_sync(Some(&Cap::Kind(Kind::NONE))));
        assert!(!accepts_full_sync(None));

        let options = lsp::TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(Kind::FULL),
            ..Default::default()
        };
        assert!(accepts_full_sync(Some(&Cap::Options(options))));
        let no_change = lsp::TextDocumentSyncOptions {
            open_close: Some(true),
            change: None,
            ..Default::default()
        };
        assert!(!accepts_full_sync(Some(&Cap::Options(no_change))));
    }

    #[test]
    fn 進捗通知を表示文字列にする() {
        let begin = lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::Begin(
            lsp::WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                message: Some("3/25".to_string()),
                percentage: Some(12),
                ..Default::default()
            },
        ));
        assert_eq!(progress_text(begin).as_deref(), Some("Indexing: 3/25 (12%)"));

        let end = lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::End(
            lsp::WorkDoneProgressEnd::default(),
        ));
        assert_eq!(progress_text(end), None);
    }

    #[test]
    fn 割合だけの進捗報告に余分な空白が付かない() {
        // 表題も本文も持たない Report は rust-analyzer が実際に送ってくる。
        let report = lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::Report(
            lsp::WorkDoneProgressReport {
                message: None,
                percentage: Some(40),
                ..Default::default()
            },
        ));
        assert_eq!(progress_text(report).as_deref(), Some("(40%)"));

        // 表題も本文も割合も無ければ表示するものが無い。
        let empty = lsp::ProgressParamsValue::WorkDone(lsp::WorkDoneProgress::Report(
            lsp::WorkDoneProgressReport::default(),
        ));
        assert_eq!(progress_text(empty), None);
    }

    #[test]
    fn 本文が読めないときは桁を変換せず行番号を保つ() {
        // 他ファイルを指す結果でファイルが消えている場合の退避経路。
        let range = lsp::Range {
            start: lsp::Position {
                line: 7,
                character: 3,
            },
            end: lsp::Position {
                line: 7,
                character: 11,
            },
        };
        assert_eq!(
            span_from(None, range),
            SpanRange::new(Position::new(7, 3), Position::new(7, 11))
        );
        // 本文があれば UTF-16 桁が char 桁に直る。8 行目の絵文字の直後は
        // UTF-16 で 11、char で 10。
        let text = "\n\n\n\n\n\n\nlet s = \"🚀ロケット\";".to_string();
        assert_eq!(
            span_from(Some(&text), range),
            SpanRange::new(Position::new(7, 3), Position::new(7, 10))
        );
    }

    #[test]
    fn サーバー発の要求には型に合った空応答を返す() {
        assert_eq!(
            empty_result("workspace/applyEdit", &Value::Null),
            json!({ "applied": false })
        );
        // 設定要求は項目数と同じ長さの配列で応じる。
        let items = json!({ "items": [{ "section": "rust-analyzer" }, { "section": "files" }] });
        assert_eq!(
            empty_result("workspace/configuration", &items),
            json!([null, null])
        );
        assert!(empty_result("window/workDoneProgress/create", &Value::Null).is_null());
        assert!(empty_result("client/registerCapability", &Value::Null).is_null());
    }

    #[tokio::test]
    async fn 未対応言語のサーバー起動は成功扱いになる() {
        let (events, _rx) = broadcast::channel(16);
        let service = LspService::new(events, DetectedTools::default());
        // 対応表に無い言語でエラーを返すと、その言語のファイルが開けなくなる。
        assert!(
            service
                .ensure_server(Path::new("/tmp"), "markdown")
                .await
                .is_ok()
        );
        assert!(service.statuses().is_empty());
    }

    #[tokio::test]
    async fn サーバーが居なければ問い合わせは空を返す() {
        let (events, _rx) = broadcast::channel(16);
        let service = LspService::new(events, DetectedTools::default());
        let path = Path::new("/tmp/nebula-lsp-test/a.rs");
        assert_eq!(service.hover(path, "fn main() {}", Position::new(0, 3)).await.unwrap(), None);
        assert!(
            service
                .completion(path, "fn main() {}", Position::new(0, 3), None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .definition(path, "fn main() {}", Position::new(0, 3))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn 起動を試みた言語は状態表に載り_イベントが流れる() {
        let (events, mut rx) = broadcast::channel(16);
        let service = LspService::new(events, DetectedTools::default());
        // pyright-langserver の有無で最終状態 (Running / NotInstalled) は変わるが、
        // どちらでも「状態が記録され GUI へ通知される」ことは変わらない。
        let _ = service.ensure_server(Path::new("/tmp"), "python").await;
        let statuses = service.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].language, "python");
        assert_eq!(statuses[0].name, "pyright-langserver");
        assert!(matches!(rx.try_recv(), Ok(Event::LspStatusChanged { .. })));
        service.shutdown().await;
    }
}
