//! 要求を対応するサービスへ振り分ける。
//!
//! ここには業務ロジックを置かない。引数の取り出しと応答への詰め直しだけを行い、
//! 実装は各サービスモジュールに閉じ込める。

use crate::state::BackendState;
use crate::{buffers, fsops, workspace};
use nebula_protocol::{PROTOCOL_VERSION, ProtocolError, Request, Response};
use std::sync::Arc;

pub async fn handle(
    state: &Arc<BackendState>,
    request: Request,
) -> Result<Response, ProtocolError> {
    use Request::*;
    match request {
        Handshake { protocol_version } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(ProtocolError::invalid(format!(
                    "プロトコル版数が違います (GUI {protocol_version} / バックエンド {PROTOCOL_VERSION})"
                )));
            }
            Ok(Response::Handshake(nebula_protocol::HandshakeInfo {
                protocol_version: PROTOCOL_VERSION,
                backend_version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                executable: state.executable.clone(),
                tools: state.tools.read().expect("検出結果のロック").clone(),
            }))
        }
        // `Shutdown` は接続層で処理されるため、ここには到達しない。
        Shutdown => Ok(Response::Ack),

        // -- ワークスペース --
        OpenWorkspace { root } => workspace::open(state, root).await.map(Response::Workspace),
        CloseWorkspace { workspace: id } => {
            workspace::close(state, id).await;
            Ok(Response::Ack)
        }
        ListWorkspaces => Ok(Response::Workspaces(workspace::list(state))),

        // -- ファイルシステム --
        ReadDir { workspace: id, path } => {
            let root = state.workspace_root(id)?;
            fsops::read_dir(&root, &path).await.map(Response::DirEntries)
        }
        CreateFile { path } => fsops::create_file(&path).await.map(|_| Response::Ack),
        CreateDir { path } => fsops::create_dir(&path).await.map(|_| Response::Ack),
        RenamePath { from, to } => fsops::rename(&from, &to).await.map(|_| Response::Ack),
        DeletePath { path, recursive } => {
            fsops::delete(&path, recursive).await.map(|_| Response::Ack)
        }
        CopyPath { from, to } => fsops::copy(&from, &to).await.map(|_| Response::Ack),
        WatchPath { workspace: id, path } => {
            state.watcher.watch(id, &path)?;
            Ok(Response::Ack)
        }

        // -- バッファ --
        OpenBuffer { workspace: id, path } => {
            buffers::open(state, id, path).await.map(Response::Buffer)
        }
        CreateScratchBuffer {
            workspace: id,
            language,
        } => Ok(Response::Buffer(buffers::create_scratch(
            state, id, language,
        ))),
        CloseBuffer { buffer } => buffers::close(state, buffer).await.map(|_| Response::Ack),
        ApplyEdits {
            buffer,
            base_version,
            edits,
        } => buffers::apply_edits(state, buffer, base_version, edits).await,
        SaveBuffer { buffer, path } => buffers::save(state, buffer, path).await,
        RequestHighlights {
            buffer,
            start_row,
            end_row,
        } => buffers::highlights(state, buffer, start_row, end_row),
        MatchingBracket { buffer, offset } => buffers::matching_bracket(state, buffer, offset),
        BufferLanguageConfig { buffer } => buffers::language_config(state, buffer),

        // -- 検索 --
        StartSearch { workspace: id, query } => {
            let root = state.workspace_root(id)?;
            state
                .search
                .start(root, query)
                .map(|search| Response::SearchStarted { search })
        }
        CancelSearch { search } => {
            state.search.cancel(search);
            Ok(Response::Ack)
        }
        FindFiles {
            workspace: id,
            query,
            limit,
        } => {
            let root = state.workspace_root(id)?;
            state
                .search
                .find_files(&root, &query, limit)
                .await
                .map(Response::FileCandidates)
        }
        ReplaceAll {
            workspace: id,
            query,
            replacement,
        } => {
            let root = state.workspace_root(id)?;
            let (files_changed, replacements) =
                state.search.replace_all(&root, &query, &replacement).await?;
            Ok(Response::ReplaceResult {
                files_changed,
                replacements,
            })
        }

        // -- Git --
        GitStatus { workspace: id } => {
            let repo = state.git_root(id)?;
            state.git.status(&repo).await.map(Response::GitStatus)
        }
        GitDiffHunks {
            workspace: id,
            path,
            contents,
        } => {
            let repo = state.git_root(id)?;
            state
                .git
                .diff_hunks(&repo, &path, contents.as_deref())
                .await
                .map(Response::GitHunks)
        }
        GitBlame { workspace: id, path } => {
            let repo = state.git_root(id)?;
            state.git.blame(&repo, &path).await.map(Response::GitBlame)
        }
        GitStage { workspace: id, paths } => {
            let repo = state.git_root(id)?;
            state.git.stage(&repo, &paths).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitUnstage { workspace: id, paths } => {
            let repo = state.git_root(id)?;
            state.git.unstage(&repo, &paths).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitDiscardChanges { workspace: id, paths } => {
            let repo = state.git_root(id)?;
            state.git.discard(&repo, &paths).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitCommit {
            workspace: id,
            message,
            amend,
        } => {
            let repo = state.git_root(id)?;
            state.git.commit(&repo, &message, amend).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitListBranches { workspace: id } => {
            let repo = state.git_root(id)?;
            state.git.branches(&repo).await.map(Response::GitBranches)
        }
        GitCheckout {
            workspace: id,
            branch,
            create,
        } => {
            let repo = state.git_root(id)?;
            state.git.checkout(&repo, &branch, create).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitLog {
            workspace: id,
            path,
            limit,
        } => {
            let repo = state.git_root(id)?;
            state
                .git
                .log(&repo, path.as_deref(), limit)
                .await
                .map(Response::GitLog)
        }
        GitFileAtHead { workspace: id, path } => {
            let repo = state.git_root(id)?;
            state
                .git
                .file_at_head(&repo, &path)
                .await
                .map(Response::GitFileContents)
        }
        GitPush { workspace: id } => {
            let repo = state.git_root(id)?;
            state.git.push(&repo).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }
        GitPull { workspace: id } => {
            let repo = state.git_root(id)?;
            state.git.pull(&repo).await?;
            emit_git_status(state, id).await;
            Ok(Response::Ack)
        }

        // -- LSP --
        LspEnsureServer { buffer } => {
            let (root, language) = buffer_language(state, buffer)?;
            state.lsp.ensure_server(&root, &language).await?;
            Ok(Response::Ack)
        }
        LspHover { buffer, position } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .hover(&path, &text, position)
                .await
                .map(Response::Hover)
        }
        LspCompletion {
            buffer,
            position,
            trigger,
        } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .completion(&path, &text, position, trigger.as_deref())
                .await
                .map(Response::Completions)
        }
        LspDefinition { buffer, position } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .definition(&path, &text, position)
                .await
                .map(Response::Locations)
        }
        LspReferences { buffer, position } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .references(&path, &text, position)
                .await
                .map(Response::Locations)
        }
        LspDocumentSymbols { buffer } => {
            let (path, _) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .document_symbols(&path)
                .await
                .map(Response::DocumentSymbols)
        }
        LspFormat { buffer } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .format(&path, &text)
                .await
                .map(Response::TextEdits)
        }
        LspRename {
            buffer,
            position,
            new_name,
        } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .rename(&path, &text, position, &new_name)
                .await
                .map(Response::WorkspaceEdit)
        }
        LspSignatureHelp { buffer, position } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .signature_help(&path, &text, position)
                .await
                .map(Response::SignatureHelp)
        }
        LspCodeActions { buffer, range } => {
            let (path, text) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .code_actions(&path, &text, range)
                .await
                .map(Response::CodeActions)
        }
        LspApplyCodeAction { buffer, index } => {
            let (path, _) = buffer_path_text(state, buffer)?;
            state
                .lsp
                .apply_code_action(&path, index)
                .await
                .map(Response::WorkspaceEdit)
        }
        LspServerStatuses { .. } => Ok(Response::LspServerStatuses(state.lsp.statuses())),
        LspRestartServer { workspace: id, language } => {
            let root = state.workspace_root(id)?;
            state.lsp.restart(&root, &language).await?;
            Ok(Response::Ack)
        }

        // -- ターミナル --
        TerminalCreate { spec } => {
            let cwd = match spec.cwd.clone() {
                Some(cwd) => Some(cwd),
                None => state.workspace_root(spec.workspace).ok(),
            };
            let spec = nebula_protocol::TerminalSpec { cwd, ..spec };
            state
                .terminals
                .create(spec)
                .map(|terminal| Response::Terminal { terminal })
        }
        TerminalInput { terminal, bytes } => {
            state.terminals.input(terminal, &bytes)?;
            Ok(Response::Ack)
        }
        TerminalResize {
            terminal,
            rows,
            cols,
        } => {
            state.terminals.resize(terminal, rows, cols)?;
            Ok(Response::Ack)
        }
        TerminalScroll {
            terminal,
            delta_lines,
        } => {
            state.terminals.scroll(terminal, delta_lines)?;
            Ok(Response::Ack)
        }
        TerminalClose { terminal } => {
            state.terminals.close(terminal)?;
            Ok(Response::Ack)
        }

        // -- Codex --
        CodexNewConversation { spec } => {
            let cwd = match spec.cwd.clone() {
                Some(cwd) => Some(cwd),
                None => state.workspace_root(spec.workspace).ok(),
            };
            let spec = nebula_protocol::CodexSessionSpec { cwd, ..spec };
            state
                .codex
                .new_conversation(spec)
                .await
                .map(|conversation| Response::CodexConversation { conversation })
        }
        CodexSendMessage {
            conversation,
            text,
            attachments,
        } => {
            state
                .codex
                .send_message(conversation, &text, &attachments)
                .await?;
            Ok(Response::Ack)
        }
        CodexRespondApproval {
            conversation,
            request_id,
            decision,
        } => {
            state
                .codex
                .respond_approval(conversation, &request_id, decision)
                .await?;
            Ok(Response::Ack)
        }
        CodexInterrupt { conversation } => {
            state.codex.interrupt(conversation).await?;
            Ok(Response::Ack)
        }
        CodexCloseConversation { conversation } => {
            state.codex.close(conversation).await?;
            Ok(Response::Ack)
        }
        CodexListModels => state.codex.list_models().await.map(Response::CodexModels),
        CodexAuthStatus => {
            let (logged_in, account) = state.codex.auth_status().await?;
            Ok(Response::CodexAuth {
                logged_in,
                account,
            })
        }
    }
}

/// バッファのパスと現在の本文を取り出す。LSP 要求はどれもこの 2 つを必要とする。
fn buffer_path_text(
    state: &Arc<BackendState>,
    buffer: nebula_protocol::BufferId,
) -> Result<(std::path::PathBuf, String), ProtocolError> {
    state.with_buffer(buffer, |entry| {
        let path = entry
            .path
            .clone()
            .ok_or_else(|| ProtocolError::invalid("ファイルに紐づかないバッファです"))?;
        Ok((path, entry.buffer.text()))
    })
}

fn buffer_language(
    state: &Arc<BackendState>,
    buffer: nebula_protocol::BufferId,
) -> Result<(std::path::PathBuf, String), ProtocolError> {
    let (workspace, language) = state.with_buffer(buffer, |entry| {
        let language = entry
            .language
            .map(|l| l.id.to_string())
            .ok_or_else(|| ProtocolError::unsupported("言語を判別できません"))?;
        Ok((entry.workspace, language))
    })?;
    Ok((state.workspace_root(workspace)?, language))
}

/// git 操作の後に最新状態を配る。GUI 側から再要求させずに済ませる。
async fn emit_git_status(state: &Arc<BackendState>, id: nebula_protocol::WorkspaceId) {
    let Ok(repo) = state.git_root(id) else {
        return;
    };
    if let Ok(status) = state.git.status(&repo).await {
        state.emit(nebula_protocol::Event::GitStatusChanged {
            workspace: id,
            status,
        });
    }
}
