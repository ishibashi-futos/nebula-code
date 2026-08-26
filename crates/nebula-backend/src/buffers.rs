//! バッファの開閉・編集・保存・ハイライト生成。
//!
//! 構文解析は編集のたびにここで走る。GUI 側では解析しないので、
//! 「軽量 GUI 描画プロセス」という要件がここで担保される。

use crate::state::{BackendState, BufferEntry};
use nebula_core::language::detect_language;
use nebula_core::{RopeExt, Selection, SyntaxTree, TextBuffer};
use nebula_protocol::{
    BufferId, BufferSnapshot, Edit, Event, HighlightSpan, LanguageConfig, LineEnding,
    ProtocolError, Response, WorkspaceId,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ファイルを開いてバッファにする。既に開いていれば同じバッファを返す。
pub async fn open(
    state: &Arc<BackendState>,
    workspace: WorkspaceId,
    path: PathBuf,
) -> Result<BufferSnapshot, ProtocolError> {
    let path = normalize(&path);
    if let Some(existing) = state.buffer_for_path(&path) {
        return state.with_buffer(existing, |entry| Ok(entry.snapshot()));
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ProtocolError::io(format!("{} を読めません: {e}", path.display())))?;
    // 不正な UTF-8 は置換文字に落として開く。読めないより読めた方がよい。
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let read_only = tokio::fs::metadata(&path)
        .await
        .map(|m| m.permissions().readonly())
        .unwrap_or(false);

    let id = BufferId::next();
    let language = detect_language(&path);
    let mut buffer = TextBuffer::new(&text);
    buffer.set_line_ending(buffer.rope().detect_line_ending());

    let syntax = build_syntax(state, language, &buffer);
    let entry = BufferEntry {
        id,
        workspace,
        path: Some(path.clone()),
        buffer,
        syntax,
        language,
        read_only,
        lsp_version: 1,
    };
    let snapshot = entry.snapshot();
    state.buffers().insert(id, entry);

    // 言語サーバーへの通知は投機的に行う。失敗しても編集はできる。
    if let Some(language) = language {
        let state = state.clone();
        let path = path.clone();
        let language_id = language.id.to_string();
        let text = snapshot.text.clone();
        let root = state.workspace_root(workspace).ok();
        tokio::spawn(async move {
            if let Some(root) = root
                && state.lsp.ensure_server(&root, &language_id).await.is_ok()
            {
                state.lsp.did_open(&path, &language_id, 1, &text).await;
            }
        });
    }
    Ok(snapshot)
}

/// ファイルに紐づかないバッファを作る。
pub fn create_scratch(
    state: &Arc<BackendState>,
    workspace: WorkspaceId,
    language_id: Option<String>,
) -> BufferSnapshot {
    let id = BufferId::next();
    let language = language_id
        .as_deref()
        .and_then(nebula_core::language::language_by_id);
    let buffer = TextBuffer::empty();
    let syntax = build_syntax(state, language, &buffer);
    let entry = BufferEntry {
        id,
        workspace,
        path: None,
        buffer,
        syntax,
        language,
        read_only: false,
        lsp_version: 1,
    };
    let snapshot = entry.snapshot();
    state.buffers().insert(id, entry);
    snapshot
}

pub async fn close(state: &Arc<BackendState>, buffer: BufferId) -> Result<(), ProtocolError> {
    let path = state.buffers().remove(&buffer).and_then(|e| e.path);
    if let Some(path) = path {
        state.lsp.did_close(&path).await;
    }
    Ok(())
}

/// 編集を適用する。構文木も同じ呼び出しの中で更新する。
pub async fn apply_edits(
    state: &Arc<BackendState>,
    buffer: BufferId,
    base_version: u64,
    edits: Vec<Edit>,
) -> Result<Response, ProtocolError> {
    let (version, path, language_id, text) = state.with_buffer(buffer, |entry| {
        if entry.read_only {
            return Err(ProtocolError::invalid("読み取り専用のバッファです"));
        }
        let selections = vec![Selection::caret(0)];
        let records = entry
            .buffer
            .apply_versioned(base_version, &edits, &selections, &selections)?;
        if let Some(syntax) = entry.syntax.as_mut() {
            for record in &records {
                syntax.apply_edit(record);
            }
            syntax.reparse(entry.buffer.rope());
        }
        entry.lsp_version += 1;
        Ok((
            entry.buffer.version(),
            entry.path.clone(),
            entry.language.map(|l| l.id.to_string()),
            entry.buffer.text(),
        ))
    })?;

    state.emit(Event::HighlightsInvalidated { buffer, version });

    if let (Some(path), Some(language_id)) = (path, language_id) {
        let lsp_version = state
            .with_buffer(buffer, |entry| Ok(entry.lsp_version))
            .unwrap_or(1);
        state
            .lsp
            .did_change(&path, &language_id, lsp_version, &text)
            .await;
    }
    Ok(Response::BufferVersion { version })
}

pub async fn save(
    state: &Arc<BackendState>,
    buffer: BufferId,
    path_override: Option<PathBuf>,
) -> Result<Response, ProtocolError> {
    let (path, text, line_ending) = state.with_buffer(buffer, |entry| {
        let path = path_override
            .clone()
            .or_else(|| entry.path.clone())
            .ok_or_else(|| ProtocolError::invalid("保存先のパスがありません"))?;
        Ok((path, entry.buffer.text(), entry.buffer.line_ending()))
    })?;

    // CRLF のファイルは CRLF のまま書き戻す。内部表現は常に元の形を保っている。
    let contents = if line_ending == LineEnding::Crlf && !text.contains("\r\n") {
        text.replace('\n', "\r\n")
    } else {
        text.clone()
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(&path, contents.as_bytes())
        .await
        .map_err(|e| ProtocolError::io(format!("{} に書けません: {e}", path.display())))?;

    let version = state.with_buffer(buffer, |entry| {
        entry.buffer.mark_saved();
        if entry.path.is_none() || path_override.is_some() {
            entry.path = Some(path.clone());
            entry.language = detect_language(&path);
        }
        Ok(entry.buffer.version())
    })?;

    state.lsp.did_save(&path, &text).await;
    Ok(Response::Saved { path, version })
}

/// 指定行範囲のハイライトを計算する。
pub fn highlights(
    state: &Arc<BackendState>,
    buffer: BufferId,
    start_row: u32,
    end_row: u32,
) -> Result<Response, ProtocolError> {
    state.with_buffer(buffer, |entry| {
        let spans: Vec<HighlightSpan> = entry
            .syntax
            .as_ref()
            .map(|syntax| syntax.highlights(entry.buffer.rope(), start_row..end_row))
            .unwrap_or_default();
        Ok(Response::Highlights {
            version: entry.buffer.version(),
            start_row,
            end_row,
            spans,
        })
    })
}

/// 対応する括弧を探す。判定は `nebula-core` の共有実装に任せる。
pub fn matching_bracket(
    state: &Arc<BackendState>,
    buffer: BufferId,
    offset: usize,
) -> Result<Response, ProtocolError> {
    state.with_buffer(buffer, |entry| {
        Ok(Response::MatchingBracket(nebula_core::matching_bracket(
            entry.buffer.rope(),
            offset,
        )))
    })
}

pub fn language_config(
    state: &Arc<BackendState>,
    buffer: BufferId,
) -> Result<Response, ProtocolError> {
    state.with_buffer(buffer, |entry| {
        Ok(Response::LanguageConfig(
            entry
                .language
                .map(|l| l.config())
                .unwrap_or_else(LanguageConfig::default),
        ))
    })
}

/// Markdown プレビュー用の要素列を組み立てる。
///
/// tree-sitter による構文解析は `highlights` と同じくここ (バックエンド) で行う。
/// GUI 側で `buffer().text()` の生成や tree-sitter を動かすと描画スレッドを塞ぐため、
/// 結果の `PreviewBlock` 列だけを IPC で渡す。
pub fn markdown_preview(
    state: &Arc<BackendState>,
    buffer: BufferId,
) -> Result<Response, ProtocolError> {
    state.with_buffer(buffer, |entry| {
        let blocks = nebula_core::markdown_preview::parse_preview(&entry.buffer.text());
        Ok(Response::MarkdownPreview {
            version: entry.buffer.version(),
            blocks,
        })
    })
}

/// 外部でファイルが変更されたときにバッファへ反映する。
///
/// 未編集のバッファだけ差し替える。編集中のものを勝手に上書きすると作業が消えるため、
/// GUI に判断を委ねる。
pub async fn reload_if_clean(state: &Arc<BackendState>, path: &Path) {
    let Some(id) = state.buffer_for_path(path) else {
        return;
    };
    let is_clean = state
        .with_buffer(id, |entry| Ok(!entry.buffer.is_dirty()))
        .unwrap_or(false);
    if !is_clean {
        return;
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let result = state.with_buffer(id, |entry| {
        if entry.buffer.text() == text {
            return Err(ProtocolError::cancelled());
        }
        entry.buffer.reset(&text);
        if let Some(syntax) = entry.syntax.as_mut() {
            syntax.reparse(entry.buffer.rope());
        }
        entry.lsp_version += 1;
        Ok(entry.buffer.version())
    });
    if let Ok(version) = result {
        state.emit(Event::BufferFileChanged {
            buffer: id,
            new_text: text,
            version,
        });
    }
}

fn build_syntax(
    state: &Arc<BackendState>,
    language: Option<&'static nebula_core::language::Language>,
    buffer: &TextBuffer,
) -> Option<SyntaxTree> {
    let language = language?;
    let compiled = state.languages.get(language).ok()?;
    let mut syntax = SyntaxTree::new(compiled).ok()?;
    syntax.reparse(buffer.rope());
    Some(syntax)
}

/// シンボリックリンクを解決せず、`.` や `..` だけを畳んだ絶対パスにする。
///
/// `canonicalize` を使わないのは、まだ存在しないファイル (新規作成直後) でも
/// 同じパスに正規化されてほしいため。
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // カレントディレクトリが取得できない (削除された作業ディレクトリなど) 稀な
        // 異常系のフォールバック。呼び出し元はこの関数にエラーを返させる作りには
        // なっておらず、影響も小さいので `Result` 化はしない。`/` 決め打ちは Unix
        // 前提で Windows では意味を持たないため、OS を問わず必ず存在する一時
        // ディレクトリを代わりの基点にする。
        std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(path)
    };
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn パス正規化が相対要素を畳む() {
        // Windows の絶対パスはドライブ文字を含んで初めて `is_absolute()` が
        // 真になる (`/a/b` はルートを持つだけで絶対パス扱いされない)。ドライブ文字
        // なしのまま `/a/b/../c/./d` を渡すと `is_absolute()` が偽になり、
        // 実行環境のカレントディレクトリを基点に結合される別の枝へ入ってしまい、
        // 期待値と一致しなくなる。相対要素を畳む、というこのテストの意図はそのまま
        // に、OS ごとに実際に絶対パスとみなされる表記へ切り替える。
        let (input, expected) = if cfg!(windows) {
            (r"C:\a\b\..\c\.\d", r"C:\a\c\d")
        } else {
            ("/a/b/../c/./d", "/a/c/d")
        };
        assert_eq!(normalize(Path::new(input)), PathBuf::from(expected));
    }

    #[test]
    fn 正規化は絶対パスを返す() {
        assert!(normalize(Path::new("relative/file.rs")).is_absolute());
    }
}
