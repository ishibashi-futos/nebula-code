//! ワークスペース (ルートフォルダ) の開閉。

use crate::state::{BackendState, Workspace};
use nebula_protocol::{Event, ProtocolError, WorkspaceId, WorkspaceInfo};
use std::path::PathBuf;
use std::sync::Arc;

pub async fn open(
    state: &Arc<BackendState>,
    root: PathBuf,
) -> Result<WorkspaceInfo, ProtocolError> {
    let root = crate::buffers::normalize(&root);
    if !tokio::fs::metadata(&root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(ProtocolError::not_found(format!(
            "{} はフォルダではありません",
            root.display()
        )));
    }

    // 同じフォルダを二重に開かない。
    if let Some(existing) = state
        .workspaces()
        .values()
        .find(|w| w.info.root == root)
        .map(|w| w.info.clone())
    {
        return Ok(existing);
    }

    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ワークスペース")
        .to_string();
    let git_root = crate::git::discover_root(&root).await;
    let info = WorkspaceInfo {
        id: WorkspaceId::next(),
        root: root.clone(),
        name,
        git_root: git_root.clone(),
    };
    state
        .workspaces()
        .insert(info.id, Workspace { info: info.clone() });

    // 監視と git 状態の取得は開いた直後に始める。GUI が要求する前に揃っている方が速い。
    if let Err(e) = state.watcher.watch(info.id, &root) {
        state.emit(Event::Notification {
            level: nebula_protocol::NotificationLevel::Warning,
            message: format!("ファイル監視を開始できません: {e}"),
        });
    }
    if let Some(repo) = git_root {
        let state = state.clone();
        let id = info.id;
        tokio::spawn(async move {
            if let Ok(status) = state.git.status(&repo).await {
                state.emit(Event::GitStatusChanged {
                    workspace: id,
                    status,
                });
            }
        });
    }
    Ok(info)
}

pub async fn close(state: &Arc<BackendState>, id: WorkspaceId) {
    state.workspaces().remove(&id);
    state.watcher.unwatch(id);
    // このワークスペースに属するバッファも閉じる。
    let orphaned: Vec<_> = state
        .buffers()
        .values()
        .filter(|b| b.workspace == id)
        .map(|b| b.id)
        .collect();
    for buffer in orphaned {
        let _ = crate::buffers::close(state, buffer).await;
    }
}

pub fn list(state: &Arc<BackendState>) -> Vec<WorkspaceInfo> {
    let mut list: Vec<_> = state.workspaces().values().map(|w| w.info.clone()).collect();
    list.sort_by_key(|w| w.id);
    list
}
