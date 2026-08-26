//! GitHub Releases から「何を」取ってくるかを担当する: 配布アセット名の解決、
//! Releases API 応答 (JSON) の解析、`curl` 経由での HTTP 取得。
//!
//! ダウンロード後の置き換え (rename・ロールバック) は `install` の責務。

use std::path::{Path, PathBuf};

use super::install::UpdateError;
use super::version::Version;

// ---------------------------------------------------------------------------
// アセット名の解決
// ---------------------------------------------------------------------------

/// `(binary, os, arch)` から配布アセット名を決める。
///
/// `.github/workflows/release.yml` のビルド成果物名と文字列で完全一致させる
/// 契約。片方だけ変更すると `nebula update` は必ず「アセットが見つかりません」
/// で失敗するようになる。命名は Node/Bun 系ツール (参考にした `overbake` 等)
/// の慣例に合わせ、`std::env::consts::OS`/`ARCH` の生の値
/// (`"macos"`/`"aarch64"`) ではなく `darwin`/`arm64` のような一般的な呼び名へ
/// 変換する。
///
/// Windows だけは配布名の末尾に `.exe` が付く (`release.yml` の `ext:` フィールド
/// を参照)。手で download した利用者がそのまま実行できるようにするための措置で、
/// 他の OS には拡張子を付けない。
pub(super) fn resolve_asset_name(binary: &str, os: &str, arch: &str) -> Option<String> {
    let platform = match (os, arch) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        ("windows", "x86_64") => "windows-x64",
        _ => return None,
    };
    let ext = if os == "windows" { ".exe" } else { "" };
    Some(format!("{binary}-{platform}{ext}"))
}

// ---------------------------------------------------------------------------
// GitHub Releases API の応答
// ---------------------------------------------------------------------------

/// self-update の情報源となる GitHub リポジトリ。ルート `Cargo.toml` の
/// `package.repository` と合わせてここが「更新先」の唯一の真実源。ズレると
/// `nebula update` は永久に 404 する。
pub(super) const REPO_SLUG: &str = "ishibashi-futos/nebula-code";

fn releases_api_url() -> String {
    format!("https://api.github.com/repos/{REPO_SLUG}/releases/latest")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReleaseAsset {
    pub(super) name: String,
    pub(super) download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReleaseInfo {
    pub(super) version: Version,
    pub(super) assets: Vec<ReleaseAsset>,
}

/// GitHub Releases API の応答 JSON のうち、必要なフィールドだけを受け取る形。
/// それ以外のフィールド (`draft`・`body` 等) は無視する (serde の既定動作)。
#[derive(Debug, serde::Deserialize)]
struct RawRelease {
    tag_name: String,
    assets: Vec<RawAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
}

/// GitHub Releases API の応答本文からタグ名とアセット一覧を取り出す。
pub(super) fn parse_release_json(body: &str) -> Result<ReleaseInfo, UpdateError> {
    let raw: RawRelease = serde_json::from_str(body).map_err(|e| {
        UpdateError::InvalidResponse(format!("リリース情報の解析に失敗しました: {e}"))
    })?;
    let version = Version::parse(&raw.tag_name).ok_or_else(|| {
        UpdateError::InvalidResponse(format!(
            "タグ名がバージョン形式ではありません: {}",
            raw.tag_name
        ))
    })?;
    let assets = raw
        .assets
        .into_iter()
        .map(|a| ReleaseAsset {
            name: a.name,
            download_url: a.browser_download_url,
        })
        .collect();
    Ok(ReleaseInfo { version, assets })
}

/// 名前が一致するアセットを探す。
pub(super) fn find_asset<'a>(assets: &'a [ReleaseAsset], name: &str) -> Option<&'a ReleaseAsset> {
    assets.iter().find(|a| a.name == name)
}

/// curl の `-w '\n%{http_code}'` が付け足した末尾の 1 行を本文から切り離す。
fn split_http_status(raw: &str) -> Result<(&str, u16), UpdateError> {
    let trimmed = raw.trim_end();
    let (body, status) = trimmed.rsplit_once('\n').ok_or_else(|| {
        UpdateError::InvalidResponse("HTTP ステータスを含む応答が得られませんでした".to_string())
    })?;
    let code = status.trim().parse::<u16>().map_err(|_| {
        UpdateError::InvalidResponse(format!("HTTP ステータスの形式が不正です: {status}"))
    })?;
    Ok((body, code))
}

// ---------------------------------------------------------------------------
// 外部コマンド呼び出し (curl)
// ---------------------------------------------------------------------------

/// GitHub API 呼び出し 1 回に許す時間 (秒)。応答はごく小さいので短くてよい。
const API_TIMEOUT_SECS: &str = "10";
/// バイナリ本体のダウンロードに許す時間 (秒)。数十MBを遅い回線で取得すること
/// を見込んで長めに取る。接続自体が確立しない場合は `--connect-timeout` の
/// 方で早期に諦める。
const DOWNLOAD_MAX_TIME_SECS: &str = "300";
const DOWNLOAD_CONNECT_TIMEOUT_SECS: &str = "10";

/// `PATH` から `curl` を探す。
///
/// `nebula-backend` の `tools::find_executable` と同じ流儀 (`which` を子プロセス
/// として起動せず自前で走査する) を踏襲するが、`nebula` クレートは
/// `nebula-backend` に依存できない (GUI とバックエンドはプロセスとして分離され、
/// IPC 経由でしかやり取りしない) ため、ここに複製している。
///
/// `tools::find_executable` と違い `PATHEXT` の候補列挙はしない — 探しているのは
/// 任意のユーザーコマンドではなく `curl` という決まった 1 本で、Windows では
/// 拡張子なしの `curl` ファイルはそもそも実行できない (`.exe` が要る) ため、
/// `std::env::consts::EXE_SUFFIX` で組み立てた `curl.exe` だけを候補にすれば足りる
/// (Windows 10 以降は標準で `curl.exe` が入っている)。
fn find_curl() -> Result<PathBuf, UpdateError> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| UpdateError::Network("PATH 環境変数が設定されていません".to_string()))?;
    let curl_name = format!("curl{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&path)
        .map(|dir| dir.join(&curl_name))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| {
            UpdateError::Network("curl が見つかりません。インストールしてください".to_string())
        })
}

/// ファイルが実行可能か。`tools::is_executable` の複製 (理由は `find_curl` を参照)。
/// Windows には実行ビットの概念が無いため、ファイルとして存在すれば実行可能と
/// みなす (`tools.rs` の同名関数と同じ判定)。
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// GitHub Releases API から最新リリースの JSON 本文を取得する。
///
/// 新規クレートを足さずに HTTPS GET するため `curl` を子プロセスとして呼ぶ。
/// 非同期ランタイムを介したタイムアウト (`tokio::time::timeout`、
/// `nebula-backend/src/tools.rs` 参照) はここでは使わない — `nebula update` は
/// `Application::new().run()` に入る前の同期コードパスで完結させる必要があり、
/// この 1 回の呼び出しのためだけに tokio を `nebula` クレートへ足すのは過剰なため、
/// `curl` 自身の `--max-time` にタイムアウトを委ねる。
///
/// `-f` は使わない。使うと 404 のとき本文もステータスも失われ、「リリース未公開」
/// と「その他の HTTP エラー」を区別できなくなる。本文とステータスは `-w` で
/// 一緒に受け取り、`split_http_status` で分離する。
pub(super) fn fetch_latest_release_body() -> Result<String, UpdateError> {
    let curl = find_curl()?;
    let output = std::process::Command::new(&curl)
        .args([
            "-sS",
            "--max-time",
            API_TIMEOUT_SECS,
            "-H",
            "Accept: application/vnd.github+json",
            "-w",
            "\n%{http_code}",
            &releases_api_url(),
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| UpdateError::Network(format!("curl を起動できません: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(UpdateError::Network(if detail.is_empty() {
            format!(
                "GitHub への接続に失敗しました (curl exit={:?})",
                output.status.code()
            )
        } else {
            format!("GitHub への接続に失敗しました: {detail}")
        }));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, status_code) = split_http_status(&stdout)?;
    match status_code {
        200 => Ok(body.to_string()),
        404 => Err(UpdateError::ReleaseNotFound),
        other => Err(UpdateError::HttpError(other)),
    }
}

/// アセットを `dest` へダウンロードする。`curl -o` に直接書き込ませ、呼び出し側
/// ではバイト列を保持しない (バイナリは数十MBあり得るため)。`-L` はここでのみ
/// 必要 (`browser_download_url` は `objects.githubusercontent.com` へ 302
/// リダイレクトする。API 呼び出しの方はリダイレクトしないため付けない)。
pub(super) fn download_to_file(url: &str, dest: &Path) -> Result<(), UpdateError> {
    let curl = find_curl()?;
    let status = std::process::Command::new(&curl)
        .args([
            "-fsSL",
            "--connect-timeout",
            DOWNLOAD_CONNECT_TIMEOUT_SECS,
            "--max-time",
            DOWNLOAD_MAX_TIME_SECS,
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| UpdateError::Network(format!("curl を起動できません: {e}")))?;

    if !status.success() {
        return Err(UpdateError::Network(format!(
            "アセットのダウンロードに失敗しました (curl exit={:?})",
            status.code()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("パースできるはずの文字列: {s}"))
    }

    // --- アセット名の解決 ---

    #[test]
    fn macos_arm64のアセット名を解決できる() {
        assert_eq!(
            resolve_asset_name("nebula", "macos", "aarch64"),
            Some("nebula-darwin-arm64".to_string())
        );
    }

    #[test]
    fn macos_x64のアセット名を解決できる() {
        assert_eq!(
            resolve_asset_name("nebula", "macos", "x86_64"),
            Some("nebula-darwin-x64".to_string())
        );
    }

    #[test]
    fn linux_x64のアセット名を解決できる() {
        assert_eq!(
            resolve_asset_name("nebula-backend", "linux", "x86_64"),
            Some("nebula-backend-linux-x64".to_string())
        );
    }

    #[test]
    fn 未対応の組み合わせはnoneになる() {
        assert_eq!(resolve_asset_name("nebula", "windows", "aarch64"), None);
        assert_eq!(resolve_asset_name("nebula", "linux", "aarch64"), None);
    }

    #[test]
    fn windows_x64のアセット名はexe拡張子が付く() {
        assert_eq!(
            resolve_asset_name("nebula", "windows", "x86_64"),
            Some("nebula-windows-x64.exe".to_string())
        );
        assert_eq!(
            resolve_asset_name("nebula-backend", "windows", "x86_64"),
            Some("nebula-backend-windows-x64.exe".to_string())
        );
    }

    /// リリース CI が作る配布名と、`resolve_asset_name` が探しに行く名前が
    /// 一致していることを、ワークフローを実際に読んで確かめる。
    ///
    /// この 2 つは文字列で完全一致していないと `nebula update` が必ず
    /// 「リリースに必要なファイルが見つかりません」で失敗するのに、
    /// 別々のファイルにあるので片方だけ直しても誰も気づけない。しかも
    /// リリースは滅多に流さないので、気づくのは配布した後になる。
    /// ここで結び付けておけば、どちらを触っても `cargo test` で分かる。
    #[test]
    fn リリースワークフローの配布プラットフォームと解決結果が一致する() {
        const WORKFLOW: &str = include_str!("../../../../.github/workflows/release.yml");

        // `resolve_asset_name` が対応している (OS, アーキテクチャ) と、
        // そこから決まるプラットフォーム名・拡張子 (Windows だけ `.exe` が付く。
        // `release.yml` の `ext:` フィールドと揃えること)。
        let supported = [
            ("macos", "aarch64", "darwin-arm64", ""),
            ("macos", "x86_64", "darwin-x64", ""),
            ("linux", "x86_64", "linux-x64", ""),
            ("windows", "x86_64", "windows-x64", ".exe"),
        ];

        for (os, arch, platform, ext) in supported {
            assert_eq!(
                resolve_asset_name("nebula", os, arch),
                Some(format!("nebula-{platform}{ext}")),
                "{os}/{arch} の解決結果が変わっている"
            );
            assert!(
                WORKFLOW.contains(&format!("platform: {platform}")),
                "{platform} を作る行がリリースワークフローに無い。\
                 update.rs が探す名前を CI が作っていないので更新が必ず失敗する"
            );
        }

        // 逆向きも見る。ワークフローにだけプラットフォームが増えていると、
        // 配布はされるのに `nebula update` からは永久に見つけられない。
        let declared = WORKFLOW.matches("platform: ").count();
        assert_eq!(
            declared,
            supported.len(),
            "ワークフローの platform 行が {} 個ある。resolve_asset_name の対応表と揃えること",
            declared
        );
    }

    // --- GitHub API の JSON 解析 ---

    /// 実際の GitHub Releases API (`GET /repos/{owner}/{repo}/releases/latest`)
    /// の応答形をした固定文字列。使わないフィールドも実物同様に含めることで、
    /// 未知フィールドを無視できることも一緒に確認する。
    const SAMPLE_RELEASE_JSON: &str = r#"{
        "url": "https://api.github.com/repos/ishibashi-futos/nebula-code/releases/12345",
        "html_url": "https://github.com/ishibashi-futos/nebula-code/releases/tag/v0.2.0",
        "id": 12345,
        "tag_name": "v0.2.0",
        "target_commitish": "main",
        "name": "v0.2.0",
        "draft": false,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": "2026-01-01T00:10:00Z",
        "assets": [
            {
                "url": "https://api.github.com/repos/ishibashi-futos/nebula-code/releases/assets/1",
                "id": 1,
                "name": "nebula-darwin-arm64",
                "content_type": "application/octet-stream",
                "size": 12345678,
                "browser_download_url": "https://github.com/ishibashi-futos/nebula-code/releases/download/v0.2.0/nebula-darwin-arm64"
            },
            {
                "url": "https://api.github.com/repos/ishibashi-futos/nebula-code/releases/assets/2",
                "id": 2,
                "name": "nebula-backend-darwin-arm64",
                "content_type": "application/octet-stream",
                "size": 8765432,
                "browser_download_url": "https://github.com/ishibashi-futos/nebula-code/releases/download/v0.2.0/nebula-backend-darwin-arm64"
            }
        ],
        "tarball_url": "https://api.github.com/repos/ishibashi-futos/nebula-code/tarball/v0.2.0",
        "zipball_url": "https://api.github.com/repos/ishibashi-futos/nebula-code/zipball/v0.2.0",
        "body": "リリースノート"
    }"#;

    #[test]
    fn 実際のgithub応答形式からタグ名とアセットを取り出せる() {
        let release = parse_release_json(SAMPLE_RELEASE_JSON).expect("解析できるはず");
        assert_eq!(release.version, v("0.2.0"));
        assert_eq!(release.assets.len(), 2);
        let asset = find_asset(&release.assets, "nebula-darwin-arm64").expect("見つかるはず");
        assert_eq!(
            asset.download_url,
            "https://github.com/ishibashi-futos/nebula-code/releases/download/v0.2.0/nebula-darwin-arm64"
        );
    }

    #[test]
    fn アセット一覧が空でも解析自体は成功する() {
        let json = r#"{"tag_name": "v1.0.0", "assets": []}"#;
        let release = parse_release_json(json).expect("解析できるはず");
        assert!(release.assets.is_empty());
        assert!(find_asset(&release.assets, "nebula-darwin-arm64").is_none());
    }

    #[test]
    fn 目的のアセットだけが欠けている場合は見つからない() {
        let json = r#"{
            "tag_name": "v1.0.0",
            "assets": [
                {"name": "nebula-linux-x64", "browser_download_url": "https://example.com/a"}
            ]
        }"#;
        let release = parse_release_json(json).expect("解析できるはず");
        assert_eq!(release.assets.len(), 1);
        assert!(find_asset(&release.assets, "nebula-darwin-arm64").is_none());
    }

    #[test]
    fn 壊れたjsonは解析エラーになる() {
        assert!(parse_release_json("{ this is not json").is_err());
    }

    #[test]
    fn 必須フィールドが欠けたjsonは解析エラーになる() {
        // GitHub の 404 応答自体もこの形 (tag_name も assets も無い)。
        let not_found =
            r#"{"message": "Not Found", "documentation_url": "https://docs.github.com"}"#;
        assert!(parse_release_json(not_found).is_err());
    }

    #[test]
    fn タグ名がバージョン形式でない場合は解析エラーになる() {
        let json = r#"{"tag_name": "not-a-version", "assets": []}"#;
        assert!(parse_release_json(json).is_err());
    }

    #[test]
    fn http応答から本文とステータスコードを分離できる() {
        let raw = "{\"tag_name\":\"v1.0.0\"}\n200";
        let (body, code) = split_http_status(raw).expect("分離できるはず");
        assert_eq!(body, "{\"tag_name\":\"v1.0.0\"}");
        assert_eq!(code, 200);
    }

    #[test]
    fn ステータス行が無い応答はエラーになる() {
        assert!(split_http_status("本文だけ").is_err());
    }

    #[test]
    fn ステータスが数値でない応答はエラーになる() {
        assert!(split_http_status("本文\nNOT_A_CODE").is_err());
    }
}
