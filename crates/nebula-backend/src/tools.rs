//! 外部ツールの検出。
//!
//! git / ripgrep / codex / 各言語サーバーは「あれば使う」方針。無い場合は該当機能を
//! 無効化して GUI に伝え、起動そのものは妨げない。検出は起動直後に 1 回だけ行う。

use nebula_protocol::DetectedTools;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

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
    let path = find_executable(name)?;
    let output = Command::new(&path)
        .arg(version_arg)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
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
}
