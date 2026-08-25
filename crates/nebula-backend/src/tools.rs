//! 外部ツールの検出。
//!
//! git / ripgrep / codex / 各言語サーバーは「あれば使う」方針。無い場合は該当機能を
//! 無効化して GUI に伝え、起動そのものは妨げない。検出は起動直後に 1 回だけ行うが、
//! ソケットを開く前には待たない。`main` がソケットを開いた **後** に `tokio::spawn`
//! で切り離して走らせ、完了したら `Event::ToolsDetected` で結果を届ける
//! (`BackendState::apply_detected_tools`)。GUI からの最初の接続を検出の遅さで
//! 待たせないため。

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
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        is_executable(&candidate).then_some(candidate)
    })
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

    #[test]
    fn 標準的な実行ファイルは見つかる() {
        // `sh` はどの Unix 環境にも存在する。
        assert!(find_executable("sh").is_some());
    }

    #[tokio::test]
    async fn 版数を返すツールは検出できる() {
        let version = probe_version_within("sh", "-c", Duration::from_secs(5)).await;
        assert!(version.is_some(), "sh は存在するので検出できる");
    }

    #[tokio::test]
    async fn 応答しないツールは制限時間で打ち切る() {
        // `sleep 5` は制限時間内に終わらない。待ち続けずに「無し」で戻ること。
        let version = probe_version_within("sleep", "5", Duration::from_millis(50)).await;
        assert!(version.is_none());
    }
}
