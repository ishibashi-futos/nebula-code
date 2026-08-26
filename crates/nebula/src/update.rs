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
//!
//! CLI 引数の解析・更新要否の判定・エントリポイントだけをここに残し、
//! バージョン比較 (`version`)・GitHub Releases API とのやり取り (`release`)・
//! 実ファイルの置き換え (`install`) はそれぞれ責務ごとに子モジュールへ
//! 分けてある。

use std::ffi::OsString;
use std::path::PathBuf;

mod install;
mod release;
mod version;

use install::UpdateError;
use release::ReleaseInfo;
use version::Version;

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
fn should_install(current: &Version, latest: &Version, force: bool) -> bool {
    force || latest.is_newer_than(current)
}

/// 引数の組み合わせから行動を決める。`--check` は「確認だけして終了」なので
/// `--force` より優先する (両方指定されても実際には何もインストールしない)。
fn plan_update(current: &Version, latest: &Version, args: UpdateArgs) -> UpdateAction {
    if args.check {
        UpdateAction::ReportOnly
    } else if should_install(current, latest, args.force) {
        UpdateAction::Install
    } else {
        UpdateAction::UpToDate
    }
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
            match install::install_release(&release) {
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
    let body = release::fetch_latest_release_body()?;
    release::parse_release_json(&body)
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
}
