//! `vX.Y.Z[-prerelease]` 形式のバージョンのパースと比較。
//!
//! 判断ロジックのうち「バージョン同士の新旧比較」だけを純粋関数として
//! 切り出してある。

/// `vX.Y.Z[-prerelease]` 形式のバージョン。GitHub Releases のタグ名比較専用の
/// 最小限の実装 (ビルドメタデータ等、フルの semver 仕様は実装しない —
/// このリポジトリのタグ運用が `vX.Y.Z` に閉じているため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

impl Version {
    /// 先頭の `v`/`V` は許容する。メジャー・マイナー・パッチの数値が 3 つ
    /// ちょうど揃っていない場合や、数値として読めない場合は `None`。
    pub(super) fn parse(s: &str) -> Option<Version> {
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
    pub(super) fn is_newer_than(&self, other: &Version) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("パースできるはずの文字列: {s}"))
    }

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
        assert!(
            Version::parse("abc").is_none(),
            "バージョンに見えない文字列"
        );
    }
}
