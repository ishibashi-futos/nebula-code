//! git 統合。
//!
//! ライブラリを抱え込まず `git` CLI を叩くのは、ユーザーの設定・フック・資格情報
//! ヘルパがそのまま効き、手元で叩いた git と挙動が食い違わないため。
//! 出力は機械可読な porcelain 形式に限定し、解析は `git/` 配下の純粋関数に分けている。

mod blame;
mod diff;
mod refs;
mod status;

use nebula_protocol::{
    BlameLine, DiffHunk, GitBranch, GitCommitInfo, GitRepoStatus, ProtocolError,
};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// 指定パスを含む git リポジトリのルート。git 管理下でなければ `None`。
pub async fn discover_root(path: &Path) -> Option<PathBuf> {
    // ファイルが渡されることがあるので、起点は必ずディレクトリにする。
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let dir = if metadata.is_dir() {
        path
    } else {
        path.parent()?
    };
    let output = run(dir, ["rev-parse", "--show-toplevel"]).await.ok()?;
    let text = String::from_utf8(output).ok()?;
    let root = text.trim_end_matches(['\n', '\r']);
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// git 操作の入口。状態を持たないのは、正本が常にリポジトリ側にあり、
/// バックエンドで持ち越してよい情報が無いため。
pub struct GitService;

impl Default for GitService {
    fn default() -> Self {
        Self::new()
    }
}

impl GitService {
    pub fn new() -> Self {
        Self
    }

    pub async fn status(&self, repo: &Path) -> Result<GitRepoStatus, ProtocolError> {
        let output = run(
            repo,
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ],
        )
        .await?;
        let mut status = status::parse(&String::from_utf8_lossy(&output));
        status.in_progress = in_progress(repo).await;
        Ok(status)
    }

    pub async fn diff_hunks(
        &self,
        repo: &Path,
        path: &Path,
        contents: Option<&str>,
    ) -> Result<Vec<DiffHunk>, ProtocolError> {
        let relative = relative(repo, path)?;
        // 比較の基準は常に HEAD にする。
        //
        // `contents` の有無で `git diff` (インデックス比較) と HEAD 比較を使い分けると、
        // ステージした瞬間に行ガターの印が消えてしまう。ガターが答えるべきは
        // 「最後のコミットから何が変わったか」なので、両経路とも HEAD と比べる。
        let current = match contents {
            Some(contents) => contents.to_string(),
            None => tokio::fs::read(path)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default(),
        };
        // HEAD に無いファイル (新規作成) は全行が追加。git の失敗をそのまま返すと
        // 新規ファイルでガターが出せなくなるので、空の HEAD 版として扱う。
        let head = show_at_head(repo, &relative).await.unwrap_or_default();
        Ok(diff::line_diff(&head, &current))
    }

    pub async fn blame(&self, repo: &Path, path: &Path) -> Result<Vec<BlameLine>, ProtocolError> {
        let relative = relative(repo, path)?;
        let pathspec = to_pathspec(&relative);
        let output = run(
            repo,
            [
                OsStr::new("blame"),
                OsStr::new("--line-porcelain"),
                OsStr::new("--"),
                pathspec.as_os_str(),
            ],
        )
        .await?;
        Ok(blame::parse(&String::from_utf8_lossy(&output)))
    }

    pub async fn stage(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), ProtocolError> {
        let paths = relatives(repo, paths)?;
        run(repo, with_paths(&["add"], &paths)).await?;
        Ok(())
    }

    pub async fn unstage(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), ProtocolError> {
        let paths = relatives(repo, paths)?;
        run(repo, with_paths(&["restore", "--staged"], &paths)).await?;
        Ok(())
    }

    pub async fn discard(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), ProtocolError> {
        let paths = relatives(repo, paths)?;
        // 未追跡ファイルは restore の対象にならず、混ぜて渡すと pathspec エラーで
        // 全体が失敗する。先に切り分けて clean に回す。
        let listed = run(
            repo,
            with_paths(&["ls-files", "--others", "--exclude-standard", "-z"], &paths),
        )
        .await?;
        let untracked: HashSet<PathBuf> = String::from_utf8_lossy(&listed)
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect();

        let (untracked, tracked): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|path| untracked.contains(path));
        if !tracked.is_empty() {
            run(repo, with_paths(&["restore"], &tracked)).await?;
        }
        if !untracked.is_empty() {
            run(repo, with_paths(&["clean", "-f", "-d"], &untracked)).await?;
        }
        Ok(())
    }

    pub async fn commit(
        &self,
        repo: &Path,
        message: &str,
        amend: bool,
    ) -> Result<(), ProtocolError> {
        let mut args = vec!["commit"];
        if amend {
            args.push("--amend");
        }
        // メッセージは引数として渡す。シェルを経由しないので引用符の心配がない。
        args.extend(["-m", message]);
        run(repo, args).await?;
        Ok(())
    }

    pub async fn branches(&self, repo: &Path) -> Result<Vec<GitBranch>, ProtocolError> {
        let output = run(
            repo,
            [
                "for-each-ref",
                &format!("--format={}", refs::BRANCH_FORMAT),
                "refs/heads",
                "refs/remotes",
            ],
        )
        .await?;
        Ok(refs::parse_branches(&String::from_utf8_lossy(&output)))
    }

    pub async fn checkout(
        &self,
        repo: &Path,
        branch: &str,
        create: bool,
    ) -> Result<(), ProtocolError> {
        let mut args = vec!["switch"];
        if create {
            args.push("-c");
        }
        args.push(branch);
        run(repo, args).await?;
        Ok(())
    }

    pub async fn log(
        &self,
        repo: &Path,
        path: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<GitCommitInfo>, ProtocolError> {
        let mut args = vec![
            OsString::from("log"),
            OsString::from(format!("--format={}", refs::LOG_FORMAT)),
        ];
        // 0 は「打ち切らない」の意味で扱う。
        if limit > 0 {
            args.push(OsString::from(format!("--max-count={limit}")));
        }
        if let Some(path) = path {
            args.push(OsString::from("--"));
            args.push(to_pathspec(&relative(repo, path)?));
        }
        let output = run(repo, args).await?;
        Ok(refs::parse_log(&String::from_utf8_lossy(&output)))
    }

    pub async fn file_at_head(&self, repo: &Path, path: &Path) -> Result<String, ProtocolError> {
        let relative = relative(repo, path)?;
        show_at_head(repo, &relative).await
    }

    pub async fn push(&self, repo: &Path) -> Result<(), ProtocolError> {
        run(repo, ["push"]).await?;
        Ok(())
    }

    pub async fn pull(&self, repo: &Path) -> Result<(), ProtocolError> {
        // マージコミットを勝手に作らせない。衝突の解決は明示的な操作に委ねる。
        run(repo, ["pull", "--ff-only"]).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// git の起動
// ---------------------------------------------------------------------------

/// git を起動して標準出力を返す。終了状態が失敗なら stderr をエラーに載せる。
async fn run<I, S>(repo: &Path, args: I) -> Result<Vec<u8>, ProtocolError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(repo)
        // status がインデックスをロックすると、GUI からの参照が裏で走っている
        // 別の git 操作とぶつかって待たされる。読み取りだけならロックは要らない。
        .env("GIT_OPTIONAL_LOCKS", "0")
        // 資格情報の入力を求められると push/pull が応答を返さないまま固まる。
        .env("GIT_TERMINAL_PROMPT", "0")
        // 既定では非 ASCII のパスが \344\270\ のようにエスケープされてしまう。
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ProtocolError::external(format!("git を起動できません: {e}")))?;

    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(ProtocolError::external(if stderr.is_empty() {
        format!("git が失敗しました ({})", output.status)
    } else {
        stderr
    }))
}

/// `git show HEAD:<相対パス>` の中身。
async fn show_at_head(repo: &Path, relative: &Path) -> Result<String, ProtocolError> {
    // 非 UTF-8 のファイル名でも壊さないよう、文字列連結ではなく OsString で組む。
    let mut spec = OsString::from("HEAD:");
    spec.push(to_pathspec(relative));
    let output = run(repo, [OsStr::new("show"), spec.as_os_str()]).await?;
    String::from_utf8(output)
        .map_err(|_| ProtocolError::invalid("UTF-8 として読めないファイルです"))
}

/// 進行中の操作。`.git` の中に残る目印で判定する。
async fn in_progress(repo: &Path) -> Option<String> {
    let git_dir = git_dir(repo).await;
    // rebase-merge と rebase-apply はリベースの実装方式の違いで、どちらもリベース中。
    let markers = [
        ("MERGE_HEAD", "マージ中"),
        ("rebase-merge", "リベース中"),
        ("rebase-apply", "リベース中"),
        ("CHERRY_PICK_HEAD", "チェリーピック中"),
    ];
    for (marker, label) in markers {
        if tokio::fs::try_exists(git_dir.join(marker))
            .await
            .unwrap_or(false)
        {
            return Some(label.to_string());
        }
    }
    None
}

/// リポジトリの `.git` ディレクトリ。リンクされた作業ツリーでは `.git` がファイルで、
/// 中身に実体の位置が書かれている。
async fn git_dir(repo: &Path) -> PathBuf {
    let dot_git = repo.join(".git");
    let Ok(text) = tokio::fs::read_to_string(&dot_git).await else {
        return dot_git;
    };
    match text.strip_prefix("gitdir:") {
        Some(location) => repo.join(location.trim()),
        None => dot_git,
    }
}

// ---------------------------------------------------------------------------
// パスの扱い
// ---------------------------------------------------------------------------

/// git に渡すためのリポジトリルート相対パス。
///
/// 絶対パスのまま渡しても多くの場合は通るが、作業ツリーの外を指していた場合の
/// エラーが分かりにくく、`HEAD:<path>` のように相対でしか書けない指定もあるため
/// 入口で揃える。
fn relative(repo: &Path, path: &Path) -> Result<PathBuf, ProtocolError> {
    // `is_relative()` ではなく `has_root()` の否定で判定する。Windows では
    // ドライブ文字を持たない `/etc/hosts` のようなパスが `is_absolute()` では
    // false (= 相対) 扱いになり、下の封じ込め判定を素通りして git まで届いてしまう
    // ため ( `is_relative()` は `is_absolute()` の否定でしかない)。ルートを持つ
    // 時点でリポジトリ相対のつもりではあり得ないので、`has_root()` で弾く。
    if !path.has_root() {
        return Ok(path.to_path_buf());
    }
    let repo = resolve(repo);
    let resolved = resolve(path);
    resolved
        .strip_prefix(&repo)
        .map(Path::to_path_buf)
        .map_err(|_| {
            ProtocolError::invalid(format!(
                "{} はリポジトリ {} の外にあります",
                resolved.display(),
                repo.display()
            ))
        })
}

fn relatives(repo: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>, ProtocolError> {
    if paths.is_empty() {
        return Err(ProtocolError::invalid("対象のパスが指定されていません"));
    }
    paths.iter().map(|path| relative(repo, path)).collect()
}

/// 部分コマンドとパス列を `--` で区切って並べる。`--` を挟むのは、`-` で始まる
/// ファイル名をオプションと解釈させないため。
fn with_paths(leading: &[&str], paths: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = leading.iter().map(OsString::from).collect();
    args.push(OsString::from("--"));
    args.extend(paths.iter().map(|path| to_pathspec(path)));
    args
}

/// リポジトリ相対パスを git に渡す pathspec (常にスラッシュ区切り) に変換する。
///
/// git はパス区切りとして `/` しか解釈せず、`\` は区切りではなく 1 文字として扱う。
/// `relative()` が返す `PathBuf` は `resolve()` の canonicalize を経由しており、
/// Windows では区切りがすべて OS 標準の `\` に揃ってしまう (元の呼び出し元が
/// `/` 区切りで渡していても関係ない)。素通しで `OsStr` 化すると
/// `git show HEAD:サブ\a.txt` のように壊れた指定になるため、git に渡す直前に
/// 必ずここを通す。
fn to_pathspec(path: &Path) -> OsString {
    // 置き換えは Windows でだけ行う。`\` が区切りだと言い切れるのはそこだけで、
    // Unix ではファイル名に literal な `\` を含められる。無条件に変換すると
    // `a\b` という 1 つのファイルを `a/b` という 2 階層の指定に化けさせてしまう。
    to_pathspec_on(path, cfg!(windows))
}

/// `windows` が真なら `\` を区切りとみなして `/` へ均す。
///
/// 実行中の OS ではなく引数で切り替えるのは、macOS 上の `cargo test` でも
/// Windows 側の挙動を検証できるようにするため。`Path::components()` で分解し直さず
/// 生バイト列を直接書き換えるのも同じ理由で、`components()` の区切り判定は
/// 実行中の OS に固定されており macOS からは Windows 形式のパスを扱えない。
/// `OsStr` のエンコーディングは ASCII を常にそのまま表す
/// ([`OsStr::as_encoded_bytes`] の契約) ので、`\` (0x5C) を `/` (0x2F) へ
/// 置き換えるだけなら非 UTF-8 なファイル名も壊さない。
fn to_pathspec_on(path: &Path, windows: bool) -> OsString {
    if !windows {
        return path.as_os_str().to_os_string();
    }
    let bytes: Vec<u8> = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|&b| if b == b'\\' { b'/' } else { b })
        .collect();
    // SAFETY: 書き換えたのは ASCII の `\` だけで、`as_encoded_bytes` が保証する
    // 「ASCII バイトは常に自分自身を表す」性質は保たれている。よって `bytes` は
    // 引き続き有効なエンコード列であり、`from_encoded_bytes_unchecked` の要件を満たす。
    unsafe { OsString::from_encoded_bytes_unchecked(bytes) }
}

/// 存在する最も深い祖先まで遡ってシンボリックリンクを解決する。
///
/// macOS の `/tmp` → `/private/tmp` のように、GUI が持つパスと
/// `rev-parse --show-toplevel` が返すパスで表記が食い違うことがある。
/// 削除済みファイルでも使えるよう、実体が無い末尾は解決せずに繋ぎ直す。
fn resolve(path: &Path) -> PathBuf {
    let normalized = crate::buffers::normalize(path);
    let mut trailing = Vec::new();
    let mut current = normalized.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(current) {
            return trailing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                trailing.push(name.to_os_string());
                current = parent;
            }
            _ => return normalized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::{GitStatusCode, HunkKind};

    /// git を直接叩いてテスト用リポジトリを組む。利用者の global 設定に影響されないよう、
    /// 署名と身元はリポジトリローカルに固定する。
    fn init_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nebula-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("テスト用ディレクトリの作成");
        git(&dir, &["init", "-q", "-b", "main", "."]);
        git(&dir, &["config", "user.name", "テスト"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        // Windows の git は既定で autocrlf 相当の動作をし、チェックアウト時に LF を
        // CRLF へ変換する。テストの意図は改行コードの検証ではなく内容の一致なので、
        // テスト用リポジトリではこの変換を止める。本番の `nebula` 側で autocrlf を
        // 無効化しているわけではない (Windows で CRLF のファイルを扱えること自体は
        // `nebula-core` の `detect_line_ending` が別途担っている)。
        git(&dir, &["config", "core.autocrlf", "false"]);
        dir
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git の起動");
        assert!(
            output.status.success(),
            "git {args:?} が失敗しました: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("親フォルダの作成");
        }
        std::fs::write(path, contents).expect("テスト用ファイルの書き込み");
    }

    #[tokio::test]
    async fn リポジトリルートを見つけられる() {
        let dir = init_repo("discover");
        write(&dir, "src/a.txt", "a\n");

        let from_file = discover_root(&dir.join("src/a.txt")).await.unwrap();
        let from_dir = discover_root(&dir.join("src")).await.unwrap();
        assert_eq!(from_file, from_dir);
        assert_eq!(resolve(&from_file), resolve(&dir));

        let outside = std::env::temp_dir().join(format!("nebula-git-none-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(discover_root(&outside).await, None);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[tokio::test]
    async fn 状態とステージとコミットが一巡する() {
        let dir = init_repo("cycle");
        let service = GitService::new();
        write(&dir, "tracked.txt", "one\n");
        write(&dir, "サブ/日本語 名.txt", "二\n");

        let status = service.status(&dir).await.unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.in_progress, None);
        let untracked: Vec<_> = status
            .entries
            .iter()
            .filter(|e| e.worktree == GitStatusCode::Untracked)
            .map(|e| e.path.clone())
            .collect();
        assert!(untracked.contains(&PathBuf::from("tracked.txt")));
        assert!(untracked.contains(&PathBuf::from("サブ/日本語 名.txt")));

        service
            .stage(
                &dir,
                &[dir.join("tracked.txt"), dir.join("サブ/日本語 名.txt")],
            )
            .await
            .unwrap();
        let staged = service.status(&dir).await.unwrap();
        assert!(
            staged
                .entries
                .iter()
                .all(|e| e.index == GitStatusCode::Added)
        );

        service.commit(&dir, "最初のコミット", false).await.unwrap();
        assert!(service.status(&dir).await.unwrap().entries.is_empty());

        let log = service.log(&dir, None, 10).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "最初のコミット");
        assert_eq!(log[0].author, "テスト");
        assert_eq!(log[0].email, "test@example.com");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn ステージを取り消せる() {
        let dir = init_repo("unstage");
        let service = GitService::new();
        write(&dir, "a.txt", "one\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "初期", false).await.unwrap();

        write(&dir, "a.txt", "two\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        assert_eq!(
            service.status(&dir).await.unwrap().entries[0].index,
            GitStatusCode::Modified
        );

        service.unstage(&dir, &[dir.join("a.txt")]).await.unwrap();
        let entry = &service.status(&dir).await.unwrap().entries[0];
        assert_eq!(entry.index, GitStatusCode::Unmodified);
        assert_eq!(entry.worktree, GitStatusCode::Modified);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 追跡済みと未追跡をまとめて破棄できる() {
        let dir = init_repo("discard");
        let service = GitService::new();
        write(&dir, "kept.txt", "original\n");
        service.stage(&dir, &[dir.join("kept.txt")]).await.unwrap();
        service.commit(&dir, "初期", false).await.unwrap();

        write(&dir, "kept.txt", "変更\n");
        write(&dir, "new.txt", "未追跡\n");
        service
            .discard(&dir, &[dir.join("kept.txt"), dir.join("new.txt")])
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("kept.txt")).unwrap(),
            "original\n"
        );
        assert!(!dir.join("new.txt").exists());
        assert!(service.status(&dir).await.unwrap().entries.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 枝の作成と切り替えができる() {
        let dir = init_repo("branch");
        let service = GitService::new();
        write(&dir, "a.txt", "one\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "土台", false).await.unwrap();

        service.checkout(&dir, "feature", true).await.unwrap();
        let branches = service.branches(&dir).await.unwrap();
        let feature = branches.iter().find(|b| b.name == "feature").unwrap();
        assert!(feature.is_head);
        assert!(!feature.is_remote);
        assert_eq!(feature.last_commit_summary, "土台");
        assert!(branches.iter().any(|b| b.name == "main" && !b.is_head));

        service.checkout(&dir, "main", false).await.unwrap();
        assert_eq!(
            service.status(&dir).await.unwrap().branch.as_deref(),
            Some("main")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 保存済みと未保存の差分を取れる() {
        let dir = init_repo("diff");
        let service = GitService::new();
        write(&dir, "a.txt", "l1\nl2\nl3\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "初期", false).await.unwrap();

        // ディスク上の変更は git diff 経由。
        write(&dir, "a.txt", "l1\nCHANGED\nl3\n");
        let saved = service
            .diff_hunks(&dir, &dir.join("a.txt"), None)
            .await
            .unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].kind, HunkKind::Modified);
        assert_eq!(saved[0].new_start, 1);
        assert_eq!(saved[0].removed_text, vec!["l2".to_string()]);

        // 未保存の内容は HEAD 版と自前で突き合わせる。
        let unsaved = service
            .diff_hunks(&dir, &dir.join("a.txt"), Some("l1\nl2\nl3\nl4\n"))
            .await
            .unwrap();
        assert_eq!(unsaved.len(), 1);
        assert_eq!(unsaved[0].kind, HunkKind::Added);
        assert_eq!((unsaved[0].new_start, unsaved[0].new_lines), (3, 1));

        // HEAD に無いファイルは全行が追加になる。
        let added = service
            .diff_hunks(&dir, &dir.join("b.txt"), Some("x\ny\n"))
            .await
            .unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kind, HunkKind::Added);
        assert_eq!(added[0].new_lines, 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn head時点の内容を取り出せる() {
        let dir = init_repo("show");
        let service = GitService::new();
        write(&dir, "サブ/a.txt", "元の内容\n");
        service
            .stage(&dir, &[dir.join("サブ/a.txt")])
            .await
            .unwrap();
        service.commit(&dir, "初期", false).await.unwrap();
        write(&dir, "サブ/a.txt", "書き換え\n");

        let head = service
            .file_at_head(&dir, &dir.join("サブ/a.txt"))
            .await
            .unwrap();
        assert_eq!(head, "元の内容\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 未コミット行を含むblameを取れる() {
        let dir = init_repo("blame");
        let service = GitService::new();
        write(&dir, "a.txt", "one\ntwo\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "初期", false).await.unwrap();
        write(&dir, "a.txt", "one\ntwo\nthree\n");

        let lines = service.blame(&dir, &dir.join("a.txt")).await.unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].author, "テスト");
        assert_eq!(lines[0].summary, "初期");
        assert!(!lines[0].is_uncommitted);
        assert_eq!(lines[2].line, 2);
        assert!(lines[2].is_uncommitted);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 衝突中はマージ中と報告する() {
        let dir = init_repo("conflict");
        let service = GitService::new();
        write(&dir, "c.txt", "base\n");
        service.stage(&dir, &[dir.join("c.txt")]).await.unwrap();
        service.commit(&dir, "土台", false).await.unwrap();

        git(&dir, &["checkout", "-q", "-b", "other"]);
        write(&dir, "c.txt", "other\n");
        service.stage(&dir, &[dir.join("c.txt")]).await.unwrap();
        service.commit(&dir, "他方", false).await.unwrap();

        git(&dir, &["checkout", "-q", "main"]);
        write(&dir, "c.txt", "main\n");
        service.stage(&dir, &[dir.join("c.txt")]).await.unwrap();
        service.commit(&dir, "本流", false).await.unwrap();
        // 衝突するので失敗する。失敗すること自体が前提。
        let _ = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["merge", "other"])
            .output()
            .expect("git merge の起動");

        let status = service.status(&dir).await.unwrap();
        assert_eq!(status.in_progress.as_deref(), Some("マージ中"));
        assert_eq!(status.entries[0].worktree, GitStatusCode::Conflicted);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 上流に対するpushとpullができる() {
        let dir = init_repo("remote");
        let service = GitService::new();
        // ネットワークを使わずに上流を用意するため、ベアリポジトリをローカルに置く。
        let origin = dir.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main", "."]);
        git(&dir, &["remote", "add", "origin", origin.to_str().unwrap()]);

        write(&dir, "a.txt", "one\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "初期", false).await.unwrap();
        git(&dir, &["push", "-q", "-u", "origin", "main"]);

        write(&dir, "a.txt", "two\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "二回目", false).await.unwrap();

        let ahead = service.status(&dir).await.unwrap();
        assert_eq!(ahead.upstream.as_deref(), Some("origin/main"));
        assert_eq!((ahead.ahead, ahead.behind), (1, 0));

        service.push(&dir).await.unwrap();
        let pushed = service.status(&dir).await.unwrap();
        assert_eq!((pushed.ahead, pushed.behind), (0, 0));

        // 上流だけを進めてから pull で追いつけることを確かめる。
        let other = dir.join("other");
        git(&dir, &["clone", "-q", origin.to_str().unwrap(), "other"]);
        git(&other, &["config", "user.name", "テスト"]);
        git(&other, &["config", "user.email", "test@example.com"]);
        write(&other, "a.txt", "three\n");
        git(&other, &["commit", "-qam", "三回目"]);
        git(&other, &["push", "-q"]);

        service.pull(&dir).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "three\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 履歴をパスで絞れる() {
        let dir = init_repo("log");
        let service = GitService::new();
        write(&dir, "a.txt", "a\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "a を追加", false).await.unwrap();
        write(&dir, "b.txt", "b\n");
        service.stage(&dir, &[dir.join("b.txt")]).await.unwrap();
        service
            .commit(&dir, "b を追加\n\n本文の説明", false)
            .await
            .unwrap();

        let all = service.log(&dir, None, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].summary, "b を追加");
        assert_eq!(all[0].body, "本文の説明");
        assert!(all[0].hash.starts_with(&all[0].short_hash));

        let only_a = service.log(&dir, Some(&dir.join("a.txt")), 0).await.unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].summary, "a を追加");

        let limited = service.log(&dir, None, 1).await.unwrap();
        assert_eq!(limited.len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 直前のコミットを訂正できる() {
        let dir = init_repo("amend");
        let service = GitService::new();
        write(&dir, "a.txt", "a\n");
        service.stage(&dir, &[dir.join("a.txt")]).await.unwrap();
        service.commit(&dir, "誤った説明", false).await.unwrap();
        service.commit(&dir, "正しい説明", true).await.unwrap();

        let log = service.log(&dir, None, 0).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "正しい説明");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn リポジトリ外のパスは拒否される() {
        let dir = init_repo("outside");
        let service = GitService::new();
        let error = service
            .file_at_head(&dir, Path::new("/etc/hosts"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, nebula_protocol::ProtocolErrorKind::InvalidRequest);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn gitの失敗はstderrを載せて返る() {
        let dir = init_repo("failure");
        let service = GitService::new();
        let error = service.checkout(&dir, "存在しない枝", false).await.unwrap_err();
        assert_eq!(error.kind, nebula_protocol::ProtocolErrorKind::ExternalTool);
        assert!(!error.message.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `to_pathspec_on` は純粋関数なので、実行中の OS に関わらず両方の作法を
    // ここで検証できる。実際に Windows 上で `relative()` が返す値が
    // バックスラッシュ区切りになることは CI 上の実行でしか確かめられない。

    #[test]
    fn パススペック変換でバックスラッシュがスラッシュになる() {
        assert_eq!(
            to_pathspec_on(Path::new("サブ\\a.txt"), true),
            OsString::from("サブ/a.txt")
        );
    }

    #[test]
    fn パススペック変換で混在した区切りも揃う() {
        // resolve() の canonicalize とパスの再結合を経ると、Windows では
        // 元がスラッシュ区切りでも一部だけバックスラッシュに変わることがある。
        assert_eq!(
            to_pathspec_on(Path::new("サブ\\奥/a.txt"), true),
            OsString::from("サブ/奥/a.txt")
        );
    }

    #[test]
    fn パススペック変換ですでにスラッシュ区切りなら変わらない() {
        assert_eq!(
            to_pathspec_on(Path::new("サブ/a.txt"), true),
            OsString::from("サブ/a.txt")
        );
    }

    #[test]
    fn パススペック変換で区切りが無ければそのまま() {
        assert_eq!(
            to_pathspec_on(Path::new("a.txt"), true),
            OsString::from("a.txt")
        );
    }

    /// Unix ではファイル名に literal な `\` を含められる。ここまで変換すると
    /// 1 つのファイルが 2 階層の指定に化けて、別のファイルを指してしまう。
    #[test]
    fn unix_ではバックスラッシュをファイル名の一部として残す() {
        assert_eq!(
            to_pathspec_on(Path::new("a\\b.txt"), false),
            OsString::from("a\\b.txt")
        );
    }
}
