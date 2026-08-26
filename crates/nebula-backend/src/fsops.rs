//! ファイルシステム操作。
//!
//! エクスプローラーから呼ばれる。`ignore` クレートで `.gitignore` を解釈し、
//! 無視対象を淡色表示できるよう印をつけて返す。

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use nebula_protocol::{DirEntry, ProtocolError};
use std::path::Path;

/// 1 階層ぶんの一覧を返す。再帰はしない (エクスプローラーは遅延展開する)。
pub async fn read_dir(root: &Path, path: &Path) -> Result<Vec<DirEntry>, ProtocolError> {
    let root = root.to_path_buf();
    let path = crate::buffers::normalize(path);
    if !path.starts_with(&root) {
        return Err(ProtocolError::invalid("ワークスペースの外は一覧できません"));
    }
    tokio::task::spawn_blocking(move || read_dir_blocking(&root, &path))
        .await
        .map_err(|e| ProtocolError::internal(format!("一覧処理が中断されました: {e}")))?
}

fn read_dir_blocking(root: &Path, path: &Path) -> Result<Vec<DirEntry>, ProtocolError> {
    let ignore = build_ignore(root);
    let mut entries = Vec::new();
    let read = std::fs::read_dir(path)
        .map_err(|e| ProtocolError::io(format!("{} を一覧できません: {e}", path.display())))?;

    for item in read.flatten() {
        let entry_path = item.path();
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        let is_symlink = file_type.is_symlink();
        // シンボリックリンクは追跡先の種別で判定する。リンク先のフォルダも展開したいため。
        let is_dir = if is_symlink {
            std::fs::metadata(&entry_path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            file_type.is_dir()
        };
        let size = item.metadata().map(|m| m.len()).unwrap_or(0);
        let name = item.file_name().to_string_lossy().into_owned();
        let is_ignored = name == ".git"
            || ignore
                .matched_path_or_any_parents(&entry_path, is_dir)
                .is_ignore();

        entries.push(DirEntry {
            name,
            path: entry_path,
            is_dir,
            is_symlink,
            size,
            is_ignored,
        });
    }

    // フォルダが先、その中で名前順。大文字小文字を無視して並べる。
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

fn build_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(root.join(".gitignore"));
    let _ = builder.add(root.join(".ignore"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

pub async fn create_file(path: &Path) -> Result<(), ProtocolError> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Err(ProtocolError::invalid(format!(
            "{} は既に存在します",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ProtocolError::io(format!("親フォルダを作れません: {e}")))?;
    }
    tokio::fs::write(path, b"")
        .await
        .map_err(|e| ProtocolError::io(format!("{} を作れません: {e}", path.display())))
}

pub async fn create_dir(path: &Path) -> Result<(), ProtocolError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| ProtocolError::io(format!("{} を作れません: {e}", path.display())))
}

pub async fn rename(from: &Path, to: &Path) -> Result<(), ProtocolError> {
    if tokio::fs::try_exists(to).await.unwrap_or(false) {
        return Err(ProtocolError::invalid(format!(
            "{} は既に存在します",
            to.display()
        )));
    }
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::rename(from, to)
        .await
        .map_err(|e| ProtocolError::io(format!("移動できません: {e}")))
}

pub async fn delete(path: &Path, recursive: bool) -> Result<(), ProtocolError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| ProtocolError::io(format!("{} が見つかりません: {e}", path.display())))?;
    let result = if metadata.is_dir() {
        if recursive {
            tokio::fs::remove_dir_all(path).await
        } else {
            tokio::fs::remove_dir(path).await
        }
    } else {
        tokio::fs::remove_file(path).await
    };
    result.map_err(|e| ProtocolError::io(format!("{} を削除できません: {e}", path.display())))
}

pub async fn copy(from: &Path, to: &Path) -> Result<(), ProtocolError> {
    let metadata = tokio::fs::symlink_metadata(from)
        .await
        .map_err(|e| ProtocolError::io(format!("{} が見つかりません: {e}", from.display())))?;
    if metadata.is_dir() {
        let from = from.to_path_buf();
        let to = to.to_path_buf();
        return tokio::task::spawn_blocking(move || copy_dir_blocking(&from, &to))
            .await
            .map_err(|e| ProtocolError::internal(format!("複製が中断されました: {e}")))?;
    }
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(from, to)
        .await
        .map(|_| ())
        .map_err(|e| ProtocolError::io(format!("複製できません: {e}")))
}

fn copy_dir_blocking(from: &Path, to: &Path) -> Result<(), ProtocolError> {
    std::fs::create_dir_all(to)
        .map_err(|e| ProtocolError::io(format!("{} を作れません: {e}", to.display())))?;
    for entry in std::fs::read_dir(from)
        .map_err(|e| ProtocolError::io(e.to_string()))?
        .flatten()
    {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_dir_blocking(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| ProtocolError::io(e.to_string()))?;
        }
    }
    Ok(())
}

/// 一意なファイル名を作る。「複製」操作で `foo.rs` → `foo copy.rs` のように使う。
pub fn unique_sibling(path: &Path) -> std::path::PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    let extension = path.extension().and_then(|s| s.to_str());
    for n in 1..1000 {
        let suffix = if n == 1 {
            " copy".to_string()
        } else {
            format!(" copy {n}")
        };
        let name = match extension {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nebula-fsops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn 一覧はフォルダを先に並べる() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::create_dir(dir.join("z_folder")).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();

        let entries = read_dir(&dir, &dir).await.unwrap();
        assert_eq!(entries[0].name, "z_folder");
        assert_eq!(entries[1].name, "a.txt");
        assert_eq!(entries[2].name, "b.txt");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn gitignore_された項目に印がつく() {
        let dir = temp_dir("ignore");
        std::fs::write(dir.join(".gitignore"), "target\n").unwrap();
        std::fs::create_dir(dir.join("target")).unwrap();
        std::fs::write(dir.join("keep.rs"), "").unwrap();

        let entries = read_dir(&dir, &dir).await.unwrap();
        let target = entries.iter().find(|e| e.name == "target").unwrap();
        let keep = entries.iter().find(|e| e.name == "keep.rs").unwrap();
        assert!(target.is_ignored);
        assert!(!keep.is_ignored);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn ワークスペース外の一覧は拒否される() {
        let dir = temp_dir("escape");
        let result = read_dir(&dir, Path::new("/etc")).await;
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 既存ファイルの作成は拒否される() {
        let dir = temp_dir("create");
        let file = dir.join("a.txt");
        create_file(&file).await.unwrap();
        assert!(create_file(&file).await.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn フォルダを再帰的に複製できる() {
        let dir = temp_dir("copy");
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/nested/a.txt"), "hello").unwrap();
        copy(&dir.join("src"), &dir.join("dst")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("dst/nested/a.txt")).unwrap(),
            "hello"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 複製名が衝突を避ける() {
        let dir = temp_dir("unique");
        let original = dir.join("foo.rs");
        std::fs::write(&original, "").unwrap();
        let first = unique_sibling(&original);
        assert_eq!(first.file_name().unwrap(), "foo copy.rs");
        std::fs::write(&first, "").unwrap();
        let second = unique_sibling(&original);
        assert_eq!(second.file_name().unwrap(), "foo copy 2.rs");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
