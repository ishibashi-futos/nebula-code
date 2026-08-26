//! 外部ツールの検出。
//!
//! git / ripgrep / codex / 各言語サーバーは「あれば使う」方針。無い場合は該当機能を
//! 無効化して GUI に伝え、起動そのものは妨げない。検出は起動直後に 1 回だけ行うが、
//! ソケットを開く前には待たない。`main` がソケットを開いた **後** に `tokio::spawn`
//! で切り離して走らせ、完了したら `Event::ToolsDetected` で結果を届ける
//! (`BackendState::apply_detected_tools`)。GUI からの最初の接続を検出の遅さで
//! 待たせないため。
//!
//! 自前でライブラリを抱え込まず、見つかった実行ファイルを CLI として叩くのは、
//! (1) バイナリサイズとビルド時間を抑えられ、(2) 利用者が普段使っているものと
//! 同じ挙動・同じ設定を通せるため。git と ripgrep はどちらも機械可読な出力形式
//! (porcelain / `--json`) を持つので、CLI 越しでも解析は安定する。

use nebula_protocol::DetectedTools;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::process::Command;

/// 検出結果の共有ハンドル。
///
/// `BackendState` 本体と `LspService`・`CodexService` の 3 箇所が同じ実体を持つ。
/// 検出はソケットを開いた後に完了するため、値渡しの複製ではなく共有可変にしないと
/// 検出完了後の結果が構築時に複製済みのコピーへ反映されない。
pub type SharedTools = Arc<RwLock<DetectedTools>>;

/// 1 ツールの検出に許す時間。
///
/// 検出はソケットを開いた後に非同期で走るので、この値は「待ち受け開始までの遅延」
/// には効かない。それでも打ち切るのは、応答しないツールをいつまでも待ち続けると
/// `DetectedTools` がいつまでも埋まらず、GUI が「検出中」のまま止まって見えるため。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `PATH` から実行ファイルを探す。
///
/// `which` を起動せず自前で走査する。プロセス起動 1 回あたり数 ms かかり、
/// 検出対象の数だけ積み上がると冷間起動の予算を食うため。
///
/// Windows では拡張子を省いた指定（`git` など）だけでは実行ファイルと認識されない。
/// `git.exe` のように `PATHEXT` が示す拡張子を付けて初めて見つかるため、
/// 候補名を複数試す。Unix はファイルの実行ビットで判定できるので拡張子は無視する。
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // 拡張子の一覧を読むところだけプラットフォームで分ける。候補名の組み立ても
    // PATH の走査も両方で共通なので、ロジックが二重にならない。
    #[cfg(unix)]
    let candidates = executable_candidates(name, None);
    #[cfg(not(unix))]
    let candidates = {
        let pathext = std::env::var("PATHEXT").ok();
        executable_candidates(name, Some(pathext.as_deref().unwrap_or_default()))
    };

    std::env::split_paths(&path).find_map(|dir| {
        candidates.iter().find_map(|candidate_name| {
            let candidate = dir.join(candidate_name);
            is_executable(&candidate).then_some(candidate)
        })
    })
}

/// 実行ファイルとして試す候補名を優先順に列挙する。
///
/// 環境変数にもファイルシステムにも触れない純粋関数にしてあるので、macOS/Linux 上の
/// `cargo test` でも Windows の挙動を検証できる。
///
/// `pathext` は「この環境で実行ファイルに付く拡張子の一覧」。
/// - `None` は拡張子という概念が無い環境 (Unix)。`name` そのものだけを試す。
/// - `Some` の中身が空なら Windows の既定 `.COM;.EXE;.BAT;.CMD` を使う。
///   `PATHEXT` は通常設定されているが、無い環境でも探索が空振りしないようにする。
/// - `name` が既に拡張子を持つ場合 (`.` を含む場合) は、拡張子を付け足した候補より
///   前にまずそのままの名前を候補に入れる。呼び出し側が `node.exe` のように
///   拡張子つきで指定したときにも素通りできるようにするため。
/// - 拡張子の大文字小文字はそのまま候補名に反映する。Windows のファイル探索自体が
///   大文字小文字を区別しないので、ここで正規化する必要はない。
fn executable_candidates(name: &str, pathext: Option<&str>) -> Vec<String> {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
    let Some(pathext) = pathext else {
        return vec![name.to_string()];
    };
    let pathext = if pathext.trim().is_empty() {
        DEFAULT_PATHEXT
    } else {
        pathext
    };

    let mut candidates = Vec::new();
    if name.contains('.') {
        candidates.push(name.to_string());
    }
    candidates.extend(
        pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!("{name}{ext}")),
    );
    candidates
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// `<cmd> <version_arg>` を実行して 1 行目を版数として返す。
async fn probe_version(name: &str, version_arg: &str) -> Option<String> {
    probe_version_within(name, version_arg, PROBE_TIMEOUT).await
}

/// 制限時間つきの版数取得。時間内に終わらなければ「無し」として扱う。
async fn probe_version_within(name: &str, version_arg: &str, timeout: Duration) -> Option<String> {
    let path = find_executable(name)?;
    let run = Command::new(&path)
        .arg(version_arg)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let output = match tokio::time::timeout(timeout, run).await {
        Ok(result) => result.ok()?,
        Err(_) => {
            // どのツールで待たされたかがログに残らないと、次に同じことが起きたとき
            // プロセスを覗くまで原因が分からない。
            eprintln!("nebula-backend: {name} の版数取得が {timeout:?} で応答しません");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let first_line = text.lines().next().unwrap_or("").trim();
    Some(if first_line.is_empty() {
        path.display().to_string()
    } else {
        first_line.to_string()
    })
}

/// 全ツールを並行して検出する。
pub async fn detect_all() -> DetectedTools {
    let (git, ripgrep, codex, rust_analyzer, tsls, pyright, bun, node) = tokio::join!(
        probe_version("git", "--version"),
        probe_version("rg", "--version"),
        probe_version("codex", "--version"),
        probe_version("rust-analyzer", "--version"),
        probe_version("typescript-language-server", "--version"),
        probe_version("pyright-langserver", "--version"),
        probe_version("bun", "--version"),
        probe_version("node", "--version"),
    );
    DetectedTools {
        git,
        ripgrep,
        codex,
        rust_analyzer,
        typescript_language_server: tsls,
        pyright,
        bun,
        node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn パスにない実行ファイルは見つからない() {
        assert!(find_executable("nebula-definitely-not-a-real-binary").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn 標準的な実行ファイルは見つかる() {
        // `sh` はどの Unix 環境にも存在する。
        assert!(find_executable("sh").is_some());
    }

    #[cfg(windows)]
    #[test]
    fn 標準的な実行ファイルは見つかる() {
        // `cmd` はどの Windows 環境にも存在する。拡張子なし指定でも PATHEXT 展開で
        // `cmd.exe` を拾えることを確認する。
        assert!(find_executable("cmd").is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn 版数を返すツールは検出できる() {
        let version = probe_version_within("sh", "-c", Duration::from_secs(5)).await;
        assert!(version.is_some(), "sh は存在するので検出できる");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn 版数を返すツールは検出できる() {
        let version = probe_version_within("where", "cmd", Duration::from_secs(5)).await;
        assert!(version.is_some(), "where は存在するので検出できる");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn 応答しないツールは制限時間で打ち切る() {
        // `sleep 5` は制限時間内に終わらない。待ち続けずに「無し」で戻ること。
        let version = probe_version_within("sleep", "5", Duration::from_millis(50)).await;
        assert!(version.is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn 応答しないツールは制限時間で打ち切る() {
        // `ping localhost` は既定で複数回の応答を待つため数秒かかる。
        // 制限時間内に終わらなければ待ち続けずに「無し」で戻ること。
        let version = probe_version_within("ping", "localhost", Duration::from_millis(50)).await;
        assert!(version.is_none());
    }

    /// Unix には拡張子という概念が無い。名前をそのまま 1 つだけ試す。
    #[test]
    fn 拡張子の概念が無い環境では名前をそのまま試す() {
        assert_eq!(executable_candidates("git", None), vec!["git"]);
        // 拡張子つきの名前を渡しても、余計な候補が増えないこと。
        assert_eq!(executable_candidates("a.out", None), vec!["a.out"]);
    }

    #[test]
    fn pathextが指定されたときその順で候補が並ぶ() {
        let candidates = executable_candidates("git", Some(".BAT;.EXE;.CMD"));
        assert_eq!(candidates, vec!["git.BAT", "git.EXE", "git.CMD"]);
    }

    #[test]
    fn pathextが空のときは既定の拡張子が使われる() {
        let candidates = executable_candidates("rg", Some(""));
        assert_eq!(candidates, vec!["rg.COM", "rg.EXE", "rg.BAT", "rg.CMD"]);
    }

    #[test]
    fn 拡張子つきの名前はそのままの名前も候補に含む() {
        let candidates = executable_candidates("node.exe", Some(".EXE"));
        assert_eq!(candidates, vec!["node.exe", "node.exe.EXE"]);
    }

    #[test]
    fn pathextの大文字小文字が混在していても扱える() {
        let candidates = executable_candidates("git", Some(".eXe;.BAT"));
        assert_eq!(candidates, vec!["git.eXe", "git.BAT"]);
    }
}
