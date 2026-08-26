//! バックエンドが待ち受ける場所 (エンドポイント) の決定。
//!
//! Unix ではファイルシステム上の Unix ドメインソケット、Windows では名前付きパイプ。
//! どちらも「文字列で表される 1 つの待ち受け位置」なので、型は `PathBuf` で揃える。
//! 名前の組み立て規則だけがプラットフォームで分かれる。

use std::path::PathBuf;

/// バックエンドが待ち受ける既定の場所。
///
/// ワークスペース単位ではなくユーザー単位で 1 デーモンを共有し、
/// 複数ウィンドウから同じバックエンドに接続する。区別に使う名前は
/// プロトコル版数そのもの。版数が違えば互換性が無く、同じ待ち受けを
/// 共有してはいけないため。
pub fn default_endpoint() -> PathBuf {
    endpoint_named(&crate::PROTOCOL_VERSION.to_string())
}

/// 名前を指定してエンドポイントを組み立てる。
///
/// 既定の場所と同じ規則で名前だけを差し替えたいとき (結合テストが本番の
/// デーモンと衝突しない待ち受けを作るときなど) に使う。「このプラットフォーム
/// ではエンドポイントをどう名付けるか」の知識をここ 1 か所に閉じ込めるのが
/// 目的で、利用側が自前で `.sock` を組み立てたりパイプ名前空間を書いたり
/// しなくて済むようにしている。
///
/// Unix: `$XDG_RUNTIME_DIR` があればそれを、無ければ `$TMPDIR` を使う。
/// どちらもユーザーごとに分かれているため、名前にユーザー名は要らない。
#[cfg(unix)]
pub fn endpoint_named(name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("nebula-{name}.sock"))
}

/// Windows: 名前付きパイプの名前空間 `\\.\pipe\` はマシン全体で 1 つしかない。
/// 同じマシンに複数のユーザーがログインしていると名前が衝突し、後から起動した方が
/// 「既に別のバックエンドが動いている」と誤判定して起動を譲ってしまう。
/// Unix 側が `$XDG_RUNTIME_DIR` から無料で得ている利用者ごとの分離を、
/// ここではユーザー名を名前に混ぜることで作る。
#[cfg(windows)]
pub fn endpoint_named(name: &str) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\nebula-{}-{}",
        sanitize_pipe_component(name),
        sanitize_pipe_component(
            &std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string())
        )
    ))
}

/// パイプ名に使える形へ均す。
///
/// `\\.\pipe\` 以降にバックスラッシュを含めると別のディレクトリ階層として
/// 解釈されてしまう。ユーザー名にはドメイン付き (`DOMAIN\user`) や空白を含む
/// ものがあるため、英数字・ハイフン・アンダースコア以外はすべて `-` へ潰す。
#[cfg(windows)]
fn sanitize_pipe_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn ドメイン付きユーザー名からもパイプ名を作れる() {
        assert_eq!(
            sanitize_pipe_component(r"CORP\Yamada Taro"),
            "CORP-Yamada-Taro"
        );
    }

    #[test]
    fn 使える文字だけの名前はそのまま通す() {
        assert_eq!(sanitize_pipe_component("dev_user-01"), "dev_user-01");
    }

    #[test]
    fn 空になる名前は既定値へ落とす() {
        assert_eq!(sanitize_pipe_component(""), "unknown");
    }

    /// 既定のエンドポイントが名前付きパイプの名前空間に載っていること。
    #[test]
    fn 既定のエンドポイントはパイプ名前空間に載る() {
        let endpoint = default_endpoint();
        let name = endpoint.to_string_lossy();
        assert!(
            name.starts_with(r"\\.\pipe\"),
            "パイプ名になっていない: {name}"
        );
    }
}
