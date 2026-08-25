//! `nebula update` による自己更新。
//!
//! GitHub Releases から最新版を取得し、実行中の `nebula` とその隣に置かれた
//! `nebula-backend` の両方を新しいバイナリへ置き換える。ウィンドウは一切開かず、
//! `Application::new().run()` に入る前 (`main.rs`) で完結させる CLI の一機能として
//! 実装する (要件: CLI として結果を出して終了コードを返す)。
//!
//! 判断ロジック (引数解析・バージョン比較・アセット名解決・JSON 解析・更新要否の
//! 判定) はすべて純粋関数に切り出してある。実際にダウンロードして自己置換する
//! 経路は、このリポジトリにまだ GitHub Release が 1 つも存在しないため
//! (`.github/workflows/release.yml` も本変更で新規に作るだけで、まだ 1 度も
//! 走っていない) 実機で確認できていない。テストで担保できるのはそこまでの
//! 判断部分のみ。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// CLI 引数の解析
// ---------------------------------------------------------------------------

/// コマンドライン引数から決まる起動モード。
///
/// プロセス名を除いた引数列を受け取る (`std::env::args_os()` を直接読まず、
/// テストしやすくするため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// 通常起動。渡されたパスをフォルダとして開く候補にする
    /// (実在確認は呼び出し側 = main.rs の責務。ここでは純粋にパースだけ行う)。
    OpenEditor(Option<PathBuf>),
    /// `nebula update [--check] [--force]`。
    Update(UpdateArgs),
}

/// `nebula update` に渡されたフラグ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateArgs {
    /// 確認だけして終了する (実際の更新は行わない)。
    pub check: bool,
    /// 同じか古いバージョンでも再インストールする。
    pub force: bool,
}

const USAGE: &str = "使い方: nebula update [--check] [--force]";

/// 引数列を解析する。先頭が `update` のときだけ自己更新として扱い、それ以外は
/// 従来どおり「フォルダを開く」起動として先頭引数をそのまま返す。
///
/// 先頭引数が `update` の場合、その名前のフォルダを開く手段を失う
/// (カレントディレクトリに `update` というフォルダがあっても `./update` のように
/// 明示しないと開けない)。サブコマンド形式の CLI では一般的なトレードオフ
/// (`git status` 等も同様) であり、ここでは許容する。
pub fn parse_cli(args: &[OsString]) -> Result<Cli, String> {
    match args.first().and_then(|a| a.to_str()) {
        Some("update") => parse_update_flags(&args[1..]).map(Cli::Update),
        _ => Ok(Cli::OpenEditor(args.first().map(PathBuf::from))),
    }
}

/// `update` の後ろに続くフラグだけを解析する。認識するのは `--check` と
/// `--force` のみで、それ以外は綴りの間違いに気づけるようエラーにする
/// (黙って無視しない)。
fn parse_update_flags(args: &[OsString]) -> Result<UpdateArgs, String> {
    let mut result = UpdateArgs::default();
    for arg in args {
        match arg.to_str() {
            Some("--check") => result.check = true,
            Some("--force") => result.force = true,
            Some(other) => return Err(format!("nebula: 不明なオプションです: {other}\n{USAGE}")),
            None => return Err(format!("nebula: 不明なオプションです\n{USAGE}")),
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// バージョン比較
// ---------------------------------------------------------------------------

/// `vX.Y.Z[-prerelease]` 形式のバージョン。GitHub Releases のタグ名比較専用の
/// 最小限の実装 (ビルドメタデータ等、フルの semver 仕様は実装しない —
/// このリポジトリのタグ運用が `vX.Y.Z` に閉じているため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

impl Version {
    /// 先頭の `v`/`V` は許容する。メジャー・マイナー・パッチの数値が 3 つ
    /// ちょうど揃っていない場合や、数値として読めない場合は `None`。
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let (core, prerelease) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            // 4つ目以降のドット区切りは "vX.Y.Z" の想定外。
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    /// `self` の方が `other` より新しいか。
    pub fn is_newer_than(&self, other: &Version) -> bool {
        self > other
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.prerelease {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| {
                compare_prerelease(self.prerelease.as_deref(), other.prerelease.as_deref())
            })
    }
}

/// プレリリース識別子の優先順位比較。semver の規則を簡略化したもの:
/// 正式版 (`None`) はプレリリース (`Some`) より新しい。両方プレリリースなら
/// `.` 区切りの識別子を左から順に比較する。
fn compare_prerelease(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => compare_identifiers(a, b),
    }
}

/// `.` 区切りの識別子どうしを左から比較する。両方とも数値として読めるときは
/// 数値として (`"10" > "9"`)、そうでなければ文字列として比較する。片方が
/// 先に尽きたら、短い方を「古い」とする (semver の規則)。
fn compare_identifiers(a: &str, b: &str) -> std::cmp::Ordering {
    let mut a_parts = a.split('.');
    let mut b_parts = b.split('.');
    loop {
        let (a_ident, b_ident) = match (a_parts.next(), b_parts.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => (x, y),
        };
        let ord = match (a_ident.parse::<u64>(), b_ident.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a_ident.cmp(b_ident),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
}

// ---------------------------------------------------------------------------
// 更新要否の判定
// ---------------------------------------------------------------------------

/// `nebula update` が取るべき行動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// 既に最新版。何もしない。
    UpToDate,
    /// 確認のみ (`--check`)。新旧に関わらずインストールはしない。
    ReportOnly,
    /// ダウンロードして置き換える。
    Install,
}

/// `force` を踏まえた「インストールすべきか」の判定。`--force` は新旧に
/// 関わらず入れ直す指示であって、「新しいことにする」の意味ではないので、
/// `--check` 時の表示 (`run_update`) にはこれを使わず `Version::is_newer_than`
/// を直接使う。
pub fn should_install(current: &Version, latest: &Version, force: bool) -> bool {
    force || latest.is_newer_than(current)
}

/// 引数の組み合わせから行動を決める。`--check` は「確認だけして終了」なので
/// `--force` より優先する (両方指定されても実際には何もインストールしない)。
pub fn plan_update(current: &Version, latest: &Version, args: UpdateArgs) -> UpdateAction {
    if args.check {
        UpdateAction::ReportOnly
    } else if should_install(current, latest, args.force) {
        UpdateAction::Install
    } else {
        UpdateAction::UpToDate
    }
}

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
pub fn resolve_asset_name(binary: &str, os: &str, arch: &str) -> Option<String> {
    let platform = match (os, arch) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        _ => return None,
    };
    Some(format!("{binary}-{platform}"))
}

// ---------------------------------------------------------------------------
// GitHub Releases API の応答
// ---------------------------------------------------------------------------

/// self-update の情報源となる GitHub リポジトリ。ルート `Cargo.toml` の
/// `package.repository` と合わせてここが「更新先」の唯一の真実源。ズレると
/// `nebula update` は永久に 404 する。
const REPO_SLUG: &str = "ishibashi-futos/nebula-code";

fn releases_api_url() -> String {
    format!("https://api.github.com/repos/{REPO_SLUG}/releases/latest")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: Version,
    pub assets: Vec<ReleaseAsset>,
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
pub fn parse_release_json(body: &str) -> Result<ReleaseInfo, UpdateError> {
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
pub fn find_asset<'a>(assets: &'a [ReleaseAsset], name: &str) -> Option<&'a ReleaseAsset> {
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
// エラー
// ---------------------------------------------------------------------------

/// `nebula update` の失敗。表示文字列はそのまま `eprintln!` に渡す前提。
#[derive(Debug)]
pub enum UpdateError {
    /// GitHub Releases API が 404 を返した = まだ 1 件もリリースされていない。
    ReleaseNotFound,
    /// 404 以外の HTTP エラー。
    HttpError(u16),
    /// DNS・接続・タイムアウトなど、HTTP 応答を得る前の失敗。
    Network(String),
    /// 応答本文が期待した形式ではなかった (JSON 破損・必須フィールド欠落・
    /// タグ名がバージョン形式でない、等)。
    InvalidResponse(String),
    /// 現在の OS/アーキテクチャ向けの配布物が存在しない。
    UnsupportedPlatform { os: &'static str, arch: &'static str },
    /// リリースに必要なアセットが含まれていない。
    AssetNotFound(String),
    /// 書き込み権限が無い (EACCES/EPERM)。
    PermissionDenied(String),
    /// 上記に当てはまらない I/O エラー。
    Io(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::ReleaseNotFound => write!(
                f,
                "リリースがまだ公開されていません: https://github.com/{REPO_SLUG}/releases"
            ),
            UpdateError::HttpError(code) => {
                write!(f, "GitHub API がエラーを返しました (HTTP {code})")
            }
            UpdateError::Network(msg) => write!(f, "ネットワークエラー: {msg}"),
            UpdateError::InvalidResponse(msg) => write!(f, "リリース情報を解釈できません: {msg}"),
            UpdateError::UnsupportedPlatform { os, arch } => {
                write!(f, "この環境 ({os}/{arch}) 向けの配布物はありません")
            }
            UpdateError::AssetNotFound(name) => {
                write!(f, "リリースに必要なファイル ({name}) が見つかりません")
            }
            UpdateError::PermissionDenied(path) => {
                write!(f, "{path} への書き込み権限がありません")
            }
            UpdateError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// I/O エラーを `nebula update` 向けのエラーへ分類する。権限不足だけは他と
/// 区別してメッセージを変える。
fn classify_io_error(err: std::io::Error, path: &Path) -> UpdateError {
    if is_permission_error(&err) {
        UpdateError::PermissionDenied(path.display().to_string())
    } else {
        UpdateError::Io(format!("{}: {err}", path.display()))
    }
}

/// 生の errno を直接見て権限エラー (EACCES/EPERM) か判定する。
///
/// `std::io::ErrorKind::PermissionDenied` への対応関係は Rust のバージョンや
/// プラットフォームで揺れてきたため、`raw_os_error()` で両方を直接突き合わせる。
/// `std::io::Error::from_raw_os_error` で合成できるので、実際に権限の無い
/// ファイルを用意しなくても (CI が root で動くと権限チェック自体が効かない
/// ことがある) 単体テストできる。
fn is_permission_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EPERM)
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
fn find_curl() -> Result<PathBuf, UpdateError> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| UpdateError::Network("PATH 環境変数が設定されていません".to_string()))?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("curl"))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| {
            UpdateError::Network("curl が見つかりません。インストールしてください".to_string())
        })
}

/// ファイルが実行可能か。`tools::is_executable` の複製 (理由は `find_curl` を参照)。
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
fn fetch_latest_release_body() -> Result<String, UpdateError> {
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
fn download_to_file(url: &str, dest: &Path) -> Result<(), UpdateError> {
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

/// ダウンロードしたファイルに実行権限 (0o755) を付ける。curl はダウンロードした
/// ファイルに実行権限を付けないため必須 (`tools::is_executable` 同様
/// `PermissionsExt` を使う。前例: `nebula-backend/src/tools.rs`)。
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| classify_io_error(e, path))
}

// ---------------------------------------------------------------------------
// 置き換え本体
// ---------------------------------------------------------------------------

/// `target` を置き換えるための一時ファイルパスを決める。
///
/// 別ファイルシステム間の `rename` は失敗するため、必ず `target` と同じ
/// ディレクトリに置く。ファイル名の先頭に `.` を付けて隠しファイルにし、
/// PID を混ぜて複数の `nebula update` が同時に走っても衝突しないようにする。
fn tmp_path_for(target: &Path) -> Result<PathBuf, UpdateError> {
    let dir = target.parent().ok_or_else(|| {
        UpdateError::Io(format!("{} の置き場所を特定できません", target.display()))
    })?;
    let file_name = target.file_name().ok_or_else(|| {
        UpdateError::Io(format!("{} はファイル名を持ちません", target.display()))
    })?;
    Ok(dir.join(format!(
        ".{}.update-{}",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}

/// 1 本のバイナリを一時ファイルへダウンロードし、実行権限を付ける。失敗時は
/// 自分が作った一時ファイルを片付けてから返す (呼び出し側はそれ以前に積んだ
/// 分だけ気にすればよい)。
fn stage_binary(target: &Path, asset: &ReleaseAsset) -> Result<PathBuf, UpdateError> {
    let tmp_path = tmp_path_for(target)?;
    println!("nebula: {} をダウンロード中...", asset.name);
    let result =
        download_to_file(&asset.download_url, &tmp_path).and_then(|()| set_executable(&tmp_path));
    match result {
        Ok(()) => Ok(tmp_path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// `nebula` と `nebula-backend` の両方を最新版へ置き換える。
///
/// 3 段階に分ける:
/// 1. (フェーズ0) 両方のアセットが見つかるかを、ディスクに触れる前に確認する。
/// 2. (フェーズ1) 両方を一時ファイルへダウンロードする。どちらかが失敗したら、
///    それまでに作った一時ファイルを片付けて中断する — 元の実行ファイルは
///    どちらも触っていないので無傷のまま残る。
/// 3. (フェーズ2) 両方を rename で置き換える。ここまでにダウンロードは完了して
///    おり、残るのは同一ディレクトリ内の rename だけなので失敗しにくい。
///
/// 1 本ずつ「ダウンロード→即rename」を繰り返さないのは、2 本目のダウンロードが
/// 失敗した場合に「GUI だけ新しくてバックエンドは旧版のまま」という、この
/// 自己更新機構が本来防ぎたい状態を自分で作ってしまうため。
fn install_release(release: &ReleaseInfo) -> Result<(), UpdateError> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let nebula_path = std::env::current_exe()
        .map_err(|e| UpdateError::Io(format!("実行中のパスを取得できません: {e}")))?;
    let backend_path = nebula_path
        .parent()
        .map(|dir| dir.join("nebula-backend"))
        .ok_or_else(|| UpdateError::Io("実行ファイルの場所を特定できません".to_string()))?;

    let binaries = [("nebula", nebula_path), ("nebula-backend", backend_path)];

    // フェーズ0。
    let mut plan: Vec<(PathBuf, ReleaseAsset)> = Vec::with_capacity(binaries.len());
    for (binary, target) in binaries {
        let name = resolve_asset_name(binary, os, arch)
            .ok_or(UpdateError::UnsupportedPlatform { os, arch })?;
        let asset = find_asset(&release.assets, &name)
            .ok_or_else(|| UpdateError::AssetNotFound(name.clone()))?
            .clone();
        plan.push((target, asset));
    }

    // フェーズ1。
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(plan.len());
    for (target, asset) in &plan {
        match stage_binary(target, asset) {
            Ok(tmp_path) => staged.push((target.clone(), tmp_path)),
            Err(e) => {
                for (_, tmp_path) in &staged {
                    let _ = std::fs::remove_file(tmp_path);
                }
                return Err(e);
            }
        }
    }

    // フェーズ2。
    for (target, tmp_path) in &staged {
        if let Err(e) = std::fs::rename(tmp_path, target) {
            let _ = std::fs::remove_file(tmp_path);
            return Err(classify_io_error(e, target));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// エントリポイント
// ---------------------------------------------------------------------------

/// 正常終了。最新版だった、または更新が完了した。
pub const EXIT_OK: i32 = 0;
/// `--check` で新しいバージョンが見つかった (更新はしていない)。スクリプトから
/// 検知できるよう、正常終了とは区別する。
pub const EXIT_UPDATE_AVAILABLE: i32 = 1;
/// 引数エラー・通信エラー・I/O エラーなど。
pub const EXIT_ERROR: i32 = 2;

/// `nebula update` の本体。結果を標準出力/標準エラーに日本語で表示し、
/// 終了コードを返す。
pub fn run_update(args: UpdateArgs) -> i32 {
    let current = current_version();
    println!("nebula: 現在のバージョン v{current}");

    let release = match fetch_release() {
        Ok(release) => release,
        Err(e) => {
            eprintln!("nebula: {e}");
            return EXIT_ERROR;
        }
    };

    match plan_update(&current, &release.version, args) {
        UpdateAction::UpToDate => {
            println!("nebula: 既に最新版です");
            EXIT_OK
        }
        UpdateAction::ReportOnly => {
            if release.version.is_newer_than(&current) {
                println!(
                    "nebula: 新しいバージョンがあります (v{current} → v{})",
                    release.version
                );
                EXIT_UPDATE_AVAILABLE
            } else {
                println!("nebula: 既に最新版です");
                EXIT_OK
            }
        }
        UpdateAction::Install => {
            println!("nebula: v{} へ更新します", release.version);
            match install_release(&release) {
                Ok(()) => {
                    println!("nebula: 更新が完了しました (v{})", release.version);
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("nebula: {e}");
                    EXIT_ERROR
                }
            }
        }
    }
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION は Cargo.toml の version から生成され、常に妥当な形式を持つ")
}

fn fetch_release() -> Result<ReleaseInfo, UpdateError> {
    let body = fetch_latest_release_body()?;
    parse_release_json(&body)
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("パースできるはずの文字列: {s}"))
    }

    // --- CLI 引数の解析 ---

    #[test]
    fn 引数なしはフォルダなしの通常起動になる() {
        assert_eq!(parse_cli(&[]), Ok(Cli::OpenEditor(None)));
    }

    #[test]
    fn フォルダパスは通常起動としてそのまま保持される() {
        let args = vec![OsString::from("some/folder")];
        assert_eq!(
            parse_cli(&args),
            Ok(Cli::OpenEditor(Some(PathBuf::from("some/folder"))))
        );
    }

    #[test]
    fn update単体はcheckもforceも立たない() {
        let args = vec![OsString::from("update")];
        assert_eq!(parse_cli(&args), Ok(Cli::Update(UpdateArgs::default())));
    }

    #[test]
    fn update_checkはcheckだけ立つ() {
        let args = vec![OsString::from("update"), OsString::from("--check")];
        assert_eq!(
            parse_cli(&args),
            Ok(Cli::Update(UpdateArgs {
                check: true,
                force: false
            }))
        );
    }

    #[test]
    fn update_forceはforceだけ立つ() {
        let args = vec![OsString::from("update"), OsString::from("--force")];
        assert_eq!(
            parse_cli(&args),
            Ok(Cli::Update(UpdateArgs {
                check: false,
                force: true
            }))
        );
    }

    #[test]
    fn update_check_forceは両方立つ() {
        let args = vec![
            OsString::from("update"),
            OsString::from("--check"),
            OsString::from("--force"),
        ];
        assert_eq!(
            parse_cli(&args),
            Ok(Cli::Update(UpdateArgs {
                check: true,
                force: true
            }))
        );
    }

    #[test]
    fn updateに未知のフラグがあるとエラーになる() {
        let args = vec![OsString::from("update"), OsString::from("--bogus")];
        assert!(parse_cli(&args).is_err());
    }

    // --- バージョン比較 ---

    #[test]
    fn vプレフィックスの有無に関わらず同じバージョンとして解釈される() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("V1.2.3"), v("1.2.3"));
    }

    #[test]
    fn パッチバージョンの新旧を比較できる() {
        assert!(v("1.2.4").is_newer_than(&v("1.2.3")));
        assert!(!v("1.2.3").is_newer_than(&v("1.2.4")));
    }

    #[test]
    fn マイナーバージョンの新旧を比較できる() {
        assert!(v("1.3.0").is_newer_than(&v("1.2.9")));
        assert!(!v("1.2.9").is_newer_than(&v("1.3.0")));
    }

    #[test]
    fn メジャーバージョンの新旧を比較できる() {
        assert!(v("2.0.0").is_newer_than(&v("1.9.9")));
        assert!(!v("1.9.9").is_newer_than(&v("2.0.0")));
    }

    #[test]
    fn 同じバージョンはどちらもis_newer_thanがfalseになる() {
        assert!(!v("1.2.3").is_newer_than(&v("1.2.3")));
        assert!(!v("v1.2.3").is_newer_than(&v("1.2.3")));
    }

    #[test]
    fn 桁数の異なる数値を文字列ではなく数値として比較する() {
        // 文字列比較だと "1.10.0" < "1.9.0" になってしまう ('1' < '9')。
        assert!(v("1.10.0").is_newer_than(&v("1.9.0")));
        assert!(!v("1.9.0").is_newer_than(&v("1.10.0")));
    }

    #[test]
    fn 正式版はプレリリース版より新しい() {
        assert!(v("1.0.0").is_newer_than(&v("1.0.0-beta")));
        assert!(!v("1.0.0-beta").is_newer_than(&v("1.0.0")));
    }

    #[test]
    fn プレリリース同士は識別子ごとに比較される() {
        assert!(v("1.0.0-beta").is_newer_than(&v("1.0.0-alpha")));
        assert!(v("1.0.0-alpha.2").is_newer_than(&v("1.0.0-alpha.1")));
        // 数値識別子は数値として比較する ("10" が "9" より新しい)。
        assert!(v("1.0.0-alpha.10").is_newer_than(&v("1.0.0-alpha.9")));
    }

    #[test]
    fn 不正な形式はパースに失敗する() {
        assert!(Version::parse("1.2").is_none(), "パッチが無い");
        assert!(Version::parse("1.2.3.4").is_none(), "セグメントが多すぎる");
        assert!(Version::parse("1.two.3").is_none(), "数値でない");
        assert!(Version::parse("").is_none(), "空文字列");
        assert!(Version::parse("abc").is_none(), "バージョンに見えない文字列");
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
        assert_eq!(resolve_asset_name("nebula", "windows", "x86_64"), None);
        assert_eq!(resolve_asset_name("nebula", "linux", "aarch64"), None);
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
        let not_found = r#"{"message": "Not Found", "documentation_url": "https://docs.github.com"}"#;
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

    // --- 更新要否の判定 ---

    #[test]
    fn 新しいバージョンがあれば通常時は更新する() {
        let args = UpdateArgs::default();
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.1.0"), args),
            UpdateAction::Install
        );
    }

    #[test]
    fn 同じバージョンなら通常時は更新しない() {
        let args = UpdateArgs::default();
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.0.0"), args),
            UpdateAction::UpToDate
        );
    }

    #[test]
    fn 現在より古いバージョンしか無ければ通常時は更新しない() {
        let args = UpdateArgs::default();
        assert_eq!(
            plan_update(&v("1.1.0"), &v("1.0.0"), args),
            UpdateAction::UpToDate
        );
    }

    #[test]
    fn forceを指定すると同じバージョンでも更新する() {
        let args = UpdateArgs {
            check: false,
            force: true,
        };
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.0.0"), args),
            UpdateAction::Install
        );
    }

    #[test]
    fn forceを指定すると古いバージョンでも再インストールする() {
        let args = UpdateArgs {
            check: false,
            force: true,
        };
        assert_eq!(
            plan_update(&v("1.1.0"), &v("1.0.0"), args),
            UpdateAction::Install
        );
    }

    #[test]
    fn checkはバージョンに関わらずインストールしない() {
        let check = UpdateArgs {
            check: true,
            force: false,
        };
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.1.0"), check),
            UpdateAction::ReportOnly
        );
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.0.0"), check),
            UpdateAction::ReportOnly
        );
        assert_eq!(
            plan_update(&v("1.1.0"), &v("1.0.0"), check),
            UpdateAction::ReportOnly
        );
    }

    #[test]
    fn checkとforceを両方指定してもインストールしない() {
        let both = UpdateArgs {
            check: true,
            force: true,
        };
        assert_eq!(
            plan_update(&v("1.0.0"), &v("1.0.0"), both),
            UpdateAction::ReportOnly
        );
    }

    #[test]
    fn should_installはforceかnewerのどちらかで真になる() {
        assert!(should_install(&v("1.0.0"), &v("1.1.0"), false));
        assert!(!should_install(&v("1.1.0"), &v("1.0.0"), false));
        assert!(should_install(&v("1.1.0"), &v("1.0.0"), true));
        assert!(should_install(&v("1.0.0"), &v("1.0.0"), true));
    }

    // --- 権限エラーの判定 ---

    #[test]
    fn eaccesは権限エラーとして分類される() {
        let err = std::io::Error::from_raw_os_error(libc::EACCES);
        assert!(is_permission_error(&err));
    }

    #[test]
    fn epermは権限エラーとして分類される() {
        let err = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(is_permission_error(&err));
    }

    #[test]
    fn enoentは権限エラーとして分類されない() {
        let err = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert!(!is_permission_error(&err));
    }

    #[test]
    fn classify_io_errorは権限エラーを専用メッセージにする() {
        let err = std::io::Error::from_raw_os_error(libc::EACCES);
        let classified = classify_io_error(err, Path::new("/usr/local/bin/nebula"));
        assert!(matches!(classified, UpdateError::PermissionDenied(_)));
        assert!(classified.to_string().contains("書き込み権限がありません"));
    }

    #[test]
    fn classify_io_errorは権限以外のエラーをioに分類する() {
        let err = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let classified = classify_io_error(err, Path::new("/usr/local/bin/nebula"));
        assert!(matches!(classified, UpdateError::Io(_)));
    }

    // --- 一時ファイルパス ---

    #[test]
    fn 一時ファイルは対象と同じディレクトリになる() {
        let target = Path::new("/usr/local/bin/nebula");
        let tmp = tmp_path_for(target).expect("パスを決められるはず");
        assert_eq!(tmp.parent(), Some(Path::new("/usr/local/bin")));
    }

    #[test]
    fn 一時ファイル名は対象のファイル名を先頭に隠しファイルとして含む() {
        let target = Path::new("/usr/local/bin/nebula-backend");
        let tmp = tmp_path_for(target).expect("パスを決められるはず");
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".nebula-backend.update-"));
    }

    // --- UpdateError の表示文言 ---

    #[test]
    fn release_not_foundは404であることが分かる文言になる() {
        assert!(UpdateError::ReleaseNotFound.to_string().contains("公開"));
    }

    #[test]
    fn unsupported_platformはosとarchを含む文言になる() {
        let err = UpdateError::UnsupportedPlatform {
            os: "windows",
            arch: "x86_64",
        };
        let msg = err.to_string();
        assert!(msg.contains("windows"));
        assert!(msg.contains("x86_64"));
    }
}
