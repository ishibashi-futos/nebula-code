//! 自己更新の「置き換え本体」: エラー型、権限判定、退避・置換・ロールバック。
//!
//! ディスクに触れる副作用とその失敗経路をすべて表す `UpdateError` を、
//! ネットワーク越しの取得を担う `release` モジュールと分けてここに置く。

use std::path::{Path, PathBuf};

use super::release::{
    REPO_SLUG, ReleaseAsset, ReleaseInfo, download_to_file, find_asset, resolve_asset_name,
};

// ---------------------------------------------------------------------------
// エラー
// ---------------------------------------------------------------------------

/// `nebula update` の失敗。表示文字列はそのまま `eprintln!` に渡す前提。
///
/// `release` モジュールの通信・JSON 解析エラーと、このモジュールのファイル
/// システムエラーの両方をまとめて表す一つの型。呼び出し側 (ルートの
/// `run_update`) がエラー源を区別せず一律に表示するため、2 つに分けずここへ
/// 集約してある。
#[derive(Debug)]
pub(in crate::update) enum UpdateError {
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
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },
    /// リリースに必要なアセットが含まれていない。
    AssetNotFound(String),
    /// 書き込み権限が無い (EACCES/EPERM)。
    PermissionDenied(String),
    /// 上記に当てはまらない I/O エラー。
    Io(String),
    /// フェーズ2 (退避 → 置換) の途中で失敗し、退避ファイルから元へ戻す
    /// ロールバック自体も失敗した。GUI とバックエンドが食い違ったまま
    /// 終了しかねないため、利用者が手動で復旧できる具体的な手順を
    /// メッセージに含めてある (`manual_recovery_message` が組み立てる)。
    ManualRecoveryRequired(String),
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
            UpdateError::ManualRecoveryRequired(msg) => write!(f, "{msg}"),
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

/// `err.kind()` が権限エラーかどうかで判定する。
///
/// 以前は `raw_os_error()` で Unix の生の errno (EACCES/EPERM) を直接
/// 突き合わせていたが、`libc` は `[target.'cfg(unix)'.dependencies]` にしか
/// 無く Windows では参照できない。`std::io::ErrorKind::PermissionDenied` は
/// Unix の EACCES/EPERM だけでなく Windows の ERROR_ACCESS_DENIED も同じ
/// 種類へ std 自身がマップしてくれるので、プラットフォーム別の errno を
/// 自分で列挙する必要が無く、`libc` に依存しなくてもこの判定だけで足りる。
/// `std::io::Error::from(ErrorKind::PermissionDenied)` で合成できるので、
/// 実際に権限の無いファイルを用意しなくても (CI が root で動くと権限
/// チェック自体が効かないことがある) 単体テストできる。
fn is_permission_error(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
}

/// ダウンロードしたファイルに実行権限 (0o755) を付ける。curl はダウンロードした
/// ファイルに実行権限を付けないため必須 (`tools::is_executable` 同様
/// `PermissionsExt` を使う。前例: `nebula-backend/src/tools.rs`)。
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| classify_io_error(e, path))
}

/// Windows には実行ビットの概念が無く、ダウンロードしたファイルはパスが
/// `.exe` で終わっていればそのまま実行できるので、何もせず成功を返す。
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

// ---------------------------------------------------------------------------
// 置き換え本体
// ---------------------------------------------------------------------------

/// `target` と同じディレクトリに、ファイル名の前後へ `prefix`/`suffix` を
/// 付けた別名のパスを作る (`tmp_path_for`・`backup_path_for` の共通の下請け)。
///
/// 別ファイルシステム間の `rename` は失敗するため、一時ファイル・退避
/// ファイルは必ず `target` と同じディレクトリに置く必要がある。
fn sibling_path(target: &Path, prefix: &str, suffix: &str) -> Result<PathBuf, UpdateError> {
    let dir = target.parent().ok_or_else(|| {
        UpdateError::Io(format!("{} の置き場所を特定できません", target.display()))
    })?;
    let file_name = target
        .file_name()
        .ok_or_else(|| UpdateError::Io(format!("{} はファイル名を持ちません", target.display())))?;
    Ok(dir.join(format!("{prefix}{}{suffix}", file_name.to_string_lossy())))
}

/// 前回の更新が残した退避ファイルを片付ける。
///
/// Windows では**実行中の実行ファイルを rename はできても削除はできない**。
/// 更新直後の後始末 (`replace_staged` の成功経路) は、そのとき動いている
/// nebula 自身と古いバックエンドを消せないので、`nebula.exe.old-1234` の
/// ような組が 1 回の更新につき 1 つずつインストール先に溜まっていく。
///
/// そこで次の更新の入口で掃除する。その頃には前回の更新で動いていた
/// プロセスはとうに終わっているので普通に消える。消せなければ黙って
/// 見送る — 掃除に失敗したからといって更新を止める理由は無い。
///
/// Unix では後始末がその場で成功するので、ここは毎回空振りする。
fn sweep_stale_backups(binaries: &[(&str, PathBuf)]) {
    for (_, target) in binaries {
        let (Some(dir), Some(file_name)) = (target.parent(), target.file_name()) else {
            continue;
        };
        let file_name = file_name.to_string_lossy();
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if is_backup_name(&entry.file_name().to_string_lossy(), &file_name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// `name` が `target` の退避ファイル (`<target>.old-<PID>`) か。
///
/// 末尾が数字であることまで見るのは、利用者が自分で置いた
/// `nebula.exe.old-backup` のようなファイルを巻き込んで消さないため。
fn is_backup_name(name: &str, target: &str) -> bool {
    name.strip_prefix(target)
        .and_then(|rest| rest.strip_prefix(".old-"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// `target` を置き換えるための一時ファイルパスを決める。
///
/// ファイル名の先頭に `.` を付けて隠しファイルにし、PID を混ぜて複数の
/// `nebula update` が同時に走っても衝突しないようにする。
fn tmp_path_for(target: &Path) -> Result<PathBuf, UpdateError> {
    sibling_path(target, ".", &format!(".update-{}", std::process::id()))
}

/// フェーズ2 で `target` を置き換える前に、既存のバイナリを退避しておく
/// ためのファイルパスを決める (例: `nebula.old-12345`)。
///
/// `tmp_path_for` と違って隠しファイルにはしない — ロールバック自体が失敗し
/// 利用者に手動で復旧してもらう場合、`ls` で見えた方が気付きやすいため。
fn backup_path_for(target: &Path) -> Result<PathBuf, UpdateError> {
    sibling_path(target, "", &format!(".old-{}", std::process::id()))
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

// ---------------------------------------------------------------------------
// フェーズ2: 退避してから置換する
// ---------------------------------------------------------------------------
//
// フェーズ2 は「target → backup」「tmp → target」という 2 段の rename から成る。
// 前者が失敗した時点では target は無傷 (何も進んでいない)。後者が失敗した
// 時点では target は空白になっている (元のファイルは backup に退避済み) ので、
// この本数自身も戻す対象に含めなければならない。この「どこで失敗したら
// どこまで戻すか」だけを、実際の rename を伴わない純粋関数として切り出し、
// 単体テストで固定する。

/// フェーズ2 の 1 本 (1 対の target/tmp_path) を処理していて失敗した段階。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase2Stage {
    /// 退避 (target → backup) 自体が失敗した。target はまだ元のまま。
    Backup,
    /// 退避は済んだが、置換 (tmp → target) が失敗した。target は空白になって
    /// いる (元のファイルは backup に退避済み)。
    Replace,
}

/// フェーズ2 が何本目 (0始まり) のどの段階で失敗したか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Phase2Failure {
    index: usize,
    stage: Phase2Stage,
}

/// 「置換対象が全部で何本あるか」と「どこで失敗したか (無ければ全部成功)」
/// から、退避ファイルを元へ戻すべき `staged` の添字一覧を、戻す順序
/// (後から成功した方を先に = 逆順) で返す。
///
/// 実際の rename は一切呼ばない — 「どこまで進めて、どこで失敗したか」だけ
/// を入力に取る決定ロジックをここに閉じ込めてあるので、ファイルシステムに
/// 触れずに単体テストできる。1 本失敗した時点で `replace_staged` はそれ以降
/// を試みない設計のため、`failure.index` より前はすべて成功している前提を
/// 置く。
fn rollback_plan(total: usize, failure: Option<Phase2Failure>) -> Vec<usize> {
    let Some(Phase2Failure { index, stage }) = failure else {
        return Vec::new();
    };
    debug_assert!(index < total, "失敗地点が置換対象の本数を超えている");
    let range = match stage {
        // 退避自体が失敗した本数はまだ何も変えていないので戻す対象に含めない。
        Phase2Stage::Backup => 0..index,
        // 退避は済んでいるので、失敗した本数自身も戻す対象に含める。
        Phase2Stage::Replace => 0..index + 1,
    };
    range.rev().collect()
}

/// 1 本を「退避 → 置換」した結果。
enum ReplaceOutcome {
    /// 両方成功した。退避ファイルのパスを保持する
    /// (呼び出し側が最終的に消すか、後続の失敗を受けて戻すために使う)。
    Success(PathBuf),
    /// 退避 (target → backup) 自体が失敗した。target は無傷。
    BackupFailed(UpdateError),
    /// 退避は済んだが、置換 (tmp → target) が失敗した。`backup` に退避済みの
    /// 元ファイルが残っている。
    ReplaceFailed { backup: PathBuf, error: UpdateError },
}

/// 1 本のバイナリを「既存を退避 → 新しい方を rename」で置き換える。
fn replace_one(target: &Path, tmp_path: &Path) -> ReplaceOutcome {
    let backup = match backup_path_for(target) {
        Ok(p) => p,
        Err(e) => return ReplaceOutcome::BackupFailed(e),
    };
    if let Err(e) = std::fs::rename(target, &backup) {
        return ReplaceOutcome::BackupFailed(classify_io_error(e, target));
    }
    match std::fs::rename(tmp_path, target) {
        Ok(()) => ReplaceOutcome::Success(backup),
        Err(e) => ReplaceOutcome::ReplaceFailed {
            backup,
            error: classify_io_error(e, target),
        },
    }
}

/// ロールバック中の rename 自体も失敗した場合に、元のエラーへ「利用者が
/// 手動で復旧するための具体的な手順」を付け足す。
///
/// ここで黙って握りつぶすと、GUI とバックエンドの版数が食い違ったまま誰も
/// 気付けない状態で残ってしまう (プロトコル版数の不一致でハンドシェイクが
/// 失敗し、GUI がバックエンドに接続できなくなる)。
///
/// 復旧コマンド名 (`mv`/`move`) は呼び出し側から `move_command` として渡す。
/// この関数自体はファイルシステムにもプラットフォームにも触れない純粋関数の
/// ままにしておきたい (単体テストで固定するため) ので、cfg 分岐は呼び出し側
/// (`replace_staged`) に置き、Unix 上のテストからも Windows 向けの文言を確認
/// できるようにしてある。
fn manual_recovery_message(
    original: &UpdateError,
    unrecovered: &[(PathBuf, PathBuf)],
    move_command: &str,
) -> String {
    let mut msg = format!(
        "{original}\nさらに、退避ファイルからの自動復旧にも失敗しました。\
         以下のコマンドを手動で実行して元に戻してください:\n"
    );
    for (target, backup) in unrecovered {
        msg.push_str(&format!(
            "  {move_command} {} {}\n",
            backup.display(),
            target.display()
        ));
    }
    msg
}

/// フェーズ2 本体。`staged` (target と、フェーズ1 でダウンロード済みの
/// 一時ファイルの組) を先頭から順に「退避 → 置換」する。
///
/// 途中で失敗したら `rollback_plan` に従い、それまでに成功した分だけ退避
/// ファイルから戻してからエラーを返す。戻す rename 自体も失敗した場合は、
/// 黙って壊れた状態のまま終わらせず、利用者が手動で復旧できる具体的な
/// パスと手順をエラーメッセージに含める (`manual_recovery_message`)。
fn replace_staged(staged: &[(PathBuf, PathBuf)]) -> Result<(), UpdateError> {
    let mut backups: Vec<PathBuf> = Vec::with_capacity(staged.len());
    let mut failure: Option<(Phase2Failure, UpdateError)> = None;

    for (index, (target, tmp_path)) in staged.iter().enumerate() {
        match replace_one(target, tmp_path) {
            ReplaceOutcome::Success(backup) => backups.push(backup),
            ReplaceOutcome::BackupFailed(e) => {
                // 退避に失敗しても tmp_path はダウンロード済みのまま残って
                // いるので、使われずに終わることが確定した以上ここで消す。
                let _ = std::fs::remove_file(tmp_path);
                failure = Some((
                    Phase2Failure {
                        index,
                        stage: Phase2Stage::Backup,
                    },
                    e,
                ));
                break;
            }
            ReplaceOutcome::ReplaceFailed { backup, error } => {
                backups.push(backup);
                let _ = std::fs::remove_file(tmp_path);
                failure = Some((
                    Phase2Failure {
                        index,
                        stage: Phase2Stage::Replace,
                    },
                    error,
                ));
                break;
            }
        }
    }

    let Some((failure, error)) = failure else {
        // 全部成功。退避ファイルはもう不要 (消せなくても更新自体は成功して
        // いるので黙って無視する)。
        for backup in &backups {
            let _ = std::fs::remove_file(backup);
        }
        return Ok(());
    };

    // 失敗した位置より後ろは、そもそも置き換えを試していない。第1段階で
    // ダウンロード済みの一時ファイルが残ったままになるので片付ける
    // (失敗した位置ぶんは上のループで消してある)。
    for (_, tmp_path) in staged.iter().skip(failure.index + 1) {
        let _ = std::fs::remove_file(tmp_path);
    }

    let mut unrecovered: Vec<(PathBuf, PathBuf)> = Vec::new();
    for i in rollback_plan(staged.len(), Some(failure)) {
        let (target, _) = &staged[i];
        if std::fs::rename(&backups[i], target).is_err() {
            unrecovered.push((target.clone(), backups[i].clone()));
        }
    }

    if unrecovered.is_empty() {
        Err(error)
    } else {
        let move_command = if cfg!(windows) { "move" } else { "mv" };
        Err(UpdateError::ManualRecoveryRequired(
            manual_recovery_message(&error, &unrecovered, move_command),
        ))
    }
}

/// `nebula` と `nebula-backend` の両方を最新版へ置き換える。
///
/// 3 段階に分ける:
/// 1. (フェーズ0) 両方のアセットが見つかるかを、ディスクに触れる前に確認する。
/// 2. (フェーズ1) 両方を一時ファイルへダウンロードする。どちらかが失敗したら、
///    それまでに作った一時ファイルを片付けて中断する — 元の実行ファイルは
///    どちらも触っていないので無傷のまま残る。
/// 3. (フェーズ2) 既存のバイナリを同じディレクトリへ退避してから、
///    ダウンロード済みの一時ファイルを rename で本来の場所へ置く
///    (`replace_staged`)。2 本目以降で失敗したら、それまでに置き換えた分を
///    退避ファイルから戻す — でなければ「GUI だけ新しくてバックエンドは
///    旧版のまま」という、プロトコル版数の不一致でハンドシェイクが失敗し
///    GUI がバックエンドに接続できなくなる状態のまま終わってしまう。
///
/// 1 本ずつ「ダウンロード→即rename」を繰り返さないのは、2 本目のダウンロードが
/// 失敗した場合に「GUI だけ新しくてバックエンドは旧版のまま」という、この
/// 自己更新機構が本来防ぎたい状態を自分で作ってしまうため。
pub(super) fn install_release(release: &ReleaseInfo) -> Result<(), UpdateError> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let nebula_path = std::env::current_exe()
        .map_err(|e| UpdateError::Io(format!("実行中のパスを取得できません: {e}")))?;
    let backend_path = nebula_path
        .parent()
        .map(|dir| dir.join(format!("nebula-backend{}", std::env::consts::EXE_SUFFIX)))
        .ok_or_else(|| UpdateError::Io("実行ファイルの場所を特定できません".to_string()))?;

    let binaries = [("nebula", nebula_path), ("nebula-backend", backend_path)];

    sweep_stale_backups(&binaries);

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
    replace_staged(&staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避ファイルの名前を見分けられる。
    #[test]
    fn 退避ファイルの名前を見分けられる() {
        assert!(is_backup_name("nebula.exe.old-1234", "nebula.exe"));
        assert!(is_backup_name("nebula.old-7", "nebula"));
    }

    /// 別のバイナリの退避ファイルを巻き込んで消さないこと。
    #[test]
    fn 別のバイナリの退避ファイルは自分のものと見なさない() {
        assert!(!is_backup_name("nebula-backend.exe.old-1234", "nebula.exe"));
        assert!(!is_backup_name("nebula.exe.old-1234", "nebula"));
    }

    /// 利用者が自分で置いた紛らわしい名前を消してしまわないこと。
    #[test]
    fn 数字で終わらない名前は退避ファイルと見なさない() {
        assert!(!is_backup_name("nebula.exe.old-backup", "nebula.exe"));
        assert!(!is_backup_name("nebula.exe.old-", "nebula.exe"));
        assert!(!is_backup_name("nebula.exe", "nebula.exe"));
    }

    // --- 権限エラーの判定 ---

    #[test]
    fn permission_deniedは権限エラーとして分類される() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(is_permission_error(&err));
    }

    #[test]
    fn notfoundは権限エラーとして分類されない() {
        // ENOENT 相当。権限エラーではないものの代表として使う。
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_permission_error(&err));
    }

    #[test]
    fn classify_io_errorは権限エラーを専用メッセージにする() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let classified = classify_io_error(err, Path::new("/usr/local/bin/nebula"));
        assert!(matches!(classified, UpdateError::PermissionDenied(_)));
        assert!(classified.to_string().contains("書き込み権限がありません"));
    }

    #[test]
    fn classify_io_errorは権限以外のエラーをioに分類する() {
        // ENOSPC (ディスク容量不足) 相当。
        let err = std::io::Error::from(std::io::ErrorKind::StorageFull);
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

    // --- 退避ファイルパス ---

    #[test]
    fn 退避ファイルは対象と同じディレクトリになる() {
        let target = Path::new("/usr/local/bin/nebula");
        let backup = backup_path_for(target).expect("パスを決められるはず");
        assert_eq!(backup.parent(), Some(Path::new("/usr/local/bin")));
    }

    #[test]
    fn 退避ファイル名は隠しファイルにせず対象のファイル名を先頭に含む() {
        let target = Path::new("/usr/local/bin/nebula-backend");
        let backup = backup_path_for(target).expect("パスを決められるはず");
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("nebula-backend.old-"));
        assert!(!name.starts_with('.'), "退避ファイルは隠しファイルにしない");
    }

    // --- フェーズ2 のロールバック計画 (決定ロジックの純粋関数) ---

    #[test]
    fn 全部成功したら戻す対象は無い() {
        assert_eq!(rollback_plan(2, None), Vec::<usize>::new());
    }

    #[test]
    fn 置換1本目が失敗すると1本目だけ戻す() {
        let failure = Phase2Failure {
            index: 0,
            stage: Phase2Stage::Replace,
        };
        assert_eq!(rollback_plan(2, Some(failure)), vec![0]);
    }

    #[test]
    fn 置換2本目が失敗すると2本目1本目の順に戻す() {
        // issue の再現そのもの: nebula (1本目) は置換済みで nebula-backend
        // (2本目) の置換が失敗する。1本目まで巻き戻さないと「GUI だけ新しく
        // てバックエンドは旧版のまま」というプロトコル不一致の状態が残る。
        let failure = Phase2Failure {
            index: 1,
            stage: Phase2Stage::Replace,
        };
        assert_eq!(rollback_plan(2, Some(failure)), vec![1, 0]);
    }

    #[test]
    fn 退避自体の失敗では失敗した本数自身は戻さず前段だけ戻す() {
        // 2本目の退避 (target → backup) 自体が失敗したケース。2本目は
        // まだ何も変えていないので戻す対象に含めない。1本目は既に置換済み
        // なので戻す。
        let failure = Phase2Failure {
            index: 1,
            stage: Phase2Stage::Backup,
        };
        assert_eq!(rollback_plan(2, Some(failure)), vec![0]);
    }

    // --- 手動復旧メッセージ ---

    #[test]
    fn 手動復旧メッセージに退避ファイルと戻し先のパスが両方含まれる() {
        let original = UpdateError::Io("rename失敗".to_string());
        let unrecovered = vec![(
            PathBuf::from("/usr/local/bin/nebula"),
            PathBuf::from("/usr/local/bin/nebula.old-123"),
        )];
        let msg = manual_recovery_message(&original, &unrecovered, "mv");
        assert!(msg.contains("/usr/local/bin/nebula.old-123"), "{msg}");
        assert!(msg.contains("/usr/local/bin/nebula"), "{msg}");
        assert!(msg.contains("手動"), "{msg}");
        assert!(msg.contains("mv "), "{msg}");
    }

    #[test]
    fn 手動復旧メッセージは渡されたコマンド名を使う() {
        let original = UpdateError::Io("rename失敗".to_string());
        let unrecovered = vec![(
            PathBuf::from(r"C:\nebula\nebula.exe"),
            PathBuf::from(r"C:\nebula\nebula.exe.old-123"),
        )];
        let msg = manual_recovery_message(&original, &unrecovered, "move");
        assert!(msg.contains("move "), "{msg}");
        assert!(!msg.contains("mv "), "{msg} には mv が含まれてはいけない");
    }

    // --- フェーズ2 の結合テスト (実ファイルに対して rename を行う) ---
    //
    // `nebula` クレートはバイナリのみでライブラリターゲットを持たないため、
    // `crates/nebula-backend/tests/ipc_roundtrip.rs` のような外部の `tests/`
    // ディレクトリからはこのモジュールを参照できない。そのためここでは
    // `#[cfg(test)] mod tests` の中に、実ファイルへ実際に rename を行う
    // テストとして置く。一意なディレクトリ名にするため
    // `std::env::temp_dir()` とプロセス ID を使う流儀は同ファイルに倣う。

    #[test]
    fn 両方成功すると中身が入れ替わりtmpも退避ファイルも残らない() {
        let dir =
            std::env::temp_dir().join(format!("nebula-update-happy-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作業ディレクトリの作成");

        let nebula_target = dir.join("nebula");
        let nebula_tmp = dir.join(".nebula.update-test");
        std::fs::write(&nebula_target, "旧nebula").expect("旧nebulaの作成");
        std::fs::write(&nebula_tmp, "新nebula").expect("新nebulaの作成");

        let backend_target = dir.join("nebula-backend");
        let backend_tmp = dir.join(".nebula-backend.update-test");
        std::fs::write(&backend_target, "旧backend").expect("旧backendの作成");
        std::fs::write(&backend_tmp, "新backend").expect("新backendの作成");

        let staged = vec![
            (nebula_target.clone(), nebula_tmp.clone()),
            (backend_target.clone(), backend_tmp.clone()),
        ];

        replace_staged(&staged).expect("両方成功するはず");

        assert_eq!(
            std::fs::read_to_string(&nebula_target).expect("nebulaの読み出し"),
            "新nebula"
        );
        assert_eq!(
            std::fs::read_to_string(&backend_target).expect("backendの読み出し"),
            "新backend"
        );
        assert!(!nebula_tmp.exists(), "tmpはrenameで消費されているはず");
        assert!(!backend_tmp.exists(), "tmpはrenameで消費されているはず");

        // 成功時は退避ファイルも掃除されているはず (要件1 の後半)。
        let nebula_backup = dir.join(format!("nebula.old-{}", std::process::id()));
        let backend_backup = dir.join(format!("nebula-backend.old-{}", std::process::id()));
        assert!(!nebula_backup.exists(), "成功後は退避ファイルを消すはず");
        assert!(!backend_backup.exists(), "成功後は退避ファイルを消すはず");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 置換2本目が失敗すると1本目も退避ファイルから元の中身へ戻る() {
        // issue の再現そのもの: nebula (1本目) の置換は成功し、
        // nebula-backend (2本目) の置換が失敗するケース。1本目を戻さないと
        // 「GUI だけ新しくてバックエンドは旧版のまま」というプロトコル不一致
        // で、GUI がバックエンドに接続できなくなる状態のまま終わってしまう。
        let dir =
            std::env::temp_dir().join(format!("nebula-update-rollback-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作業ディレクトリの作成");

        let nebula_target = dir.join("nebula");
        let nebula_tmp = dir.join(".nebula.update-test");
        std::fs::write(&nebula_target, "旧nebula").expect("旧nebulaの作成");
        std::fs::write(&nebula_tmp, "新nebula").expect("新nebulaの作成");

        let backend_target = dir.join("nebula-backend");
        // わざと tmp を用意しない → rename(tmp, target) が失敗し、「退避
        // (target→backup) には成功したが置換には失敗した」状況を再現する。
        let backend_tmp = dir.join(".nebula-backend.update-test");
        std::fs::write(&backend_target, "旧backend").expect("旧backendの作成");

        let staged = vec![
            (nebula_target.clone(), nebula_tmp.clone()),
            (backend_target.clone(), backend_tmp.clone()),
        ];

        let result = replace_staged(&staged);
        assert!(result.is_err(), "2本目のtmpが無いので失敗するはず");

        // 1本目 (nebula) は一度置換されたが、2本目の失敗を受けて退避ファイル
        // から元の中身へ戻っているはず。
        assert_eq!(
            std::fs::read_to_string(&nebula_target).expect("nebulaの読み出し"),
            "旧nebula",
            "1本目が退避ファイルから戻っていない"
        );
        // 2本目は退避したものを戻しただけなので中身は変わっていない。
        assert_eq!(
            std::fs::read_to_string(&backend_target).expect("backendの読み出し"),
            "旧backend"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1本目で失敗すると、2本目以降は置き換えを試す前に終わる。第1段階で
    /// ダウンロード済みの一時ファイルが残ったままだと、更新に失敗するたびに
    /// 実行ファイルの隣にゴミが積もっていく。
    #[test]
    fn 置換に失敗したら試していない分の一時ファイルも片付ける() {
        let dir =
            std::env::temp_dir().join(format!("nebula-update-tmp-gc-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作業ディレクトリの作成");

        let nebula_target = dir.join("nebula");
        // わざと tmp を用意しない → 1本目がここで失敗する。
        let nebula_tmp = dir.join(".nebula.update-test");
        std::fs::write(&nebula_target, "旧nebula").expect("旧nebulaの作成");

        let backend_target = dir.join("nebula-backend");
        let backend_tmp = dir.join(".nebula-backend.update-test");
        std::fs::write(&backend_target, "旧backend").expect("旧backendの作成");
        std::fs::write(&backend_tmp, "新backend").expect("新backendの作成");

        let staged = vec![
            (nebula_target.clone(), nebula_tmp.clone()),
            (backend_target.clone(), backend_tmp.clone()),
        ];

        let result = replace_staged(&staged);
        assert!(result.is_err(), "1本目のtmpが無いので失敗するはず");

        assert!(
            !backend_tmp.exists(),
            "試していない2本目の一時ファイルが残っている: {}",
            backend_tmp.display()
        );
        assert_eq!(
            std::fs::read_to_string(&backend_target).expect("backendの読み出し"),
            "旧backend",
            "2本目は手を付けていないので中身が変わってはいけない"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
