//! 言語定義とその遅延ロード。
//!
//! 文法本体 (`tree_sitter::Language`) は静的リンクされているので取得は安価だが、
//! ハイライトクエリのコンパイルは 1 言語あたり数ミリ秒かかる。冷間起動 200ms の予算に
//! 効くため、**実際にその言語のファイルを開いた時点で初めて**コンパイルする。

use nebula_protocol::{LanguageConfig, TokenKind};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tree_sitter::{Query, QueryError};

/// 静的に定義される言語。
#[derive(Debug, Clone, Copy)]
pub struct Language {
    /// LSP の languageId と揃えた識別子。
    pub id: &'static str,
    pub display_name: &'static str,
    pub extensions: &'static [&'static str],
    /// 拡張子を持たない特別なファイル名 (`Makefile` など)。
    pub filenames: &'static [&'static str],
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    pub indent_width: u32,
    pub use_tabs: bool,
    /// 文法を返す関数。定数畳み込みできないので関数ポインタで持つ。
    grammar: fn() -> tree_sitter::Language,
    highlights_query: &'static str,
    /// 埋め込み言語 (HTML 内の JS 等) のクエリ。未対応の言語は `None`。
    injections_query: Option<&'static str>,
}

impl Language {
    pub fn config(&self) -> LanguageConfig {
        LanguageConfig {
            language: Some(self.id.to_string()),
            indent_width: self.indent_width,
            use_tabs: self.use_tabs,
            line_comment: self.line_comment.map(str::to_string),
            block_comment: self
                .block_comment
                .map(|(a, b)| (a.to_string(), b.to_string())),
            auto_close_pairs: auto_close_pairs_for(self.id),
        }
    }
}

fn auto_close_pairs_for(id: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = ["()", "[]", "{}"]
        .iter()
        .map(|p| {
            let mut c = p.chars();
            (c.next().unwrap().to_string(), c.next().unwrap().to_string())
        })
        .collect();
    pairs.push(("\"".into(), "\"".into()));
    // Rust のライフタイム (`'a`) と Markdown のアポストロフィでは
    // 単一引用符の自動閉じが邪魔になるため除外する。
    if !matches!(id, "rust" | "markdown") {
        pairs.push(("'".into(), "'".into()));
    }
    if matches!(id, "javascript" | "typescript" | "tsx" | "markdown") {
        pairs.push(("`".into(), "`".into()));
    }
    pairs
}

/// 対応言語の一覧。
///
/// 追加は `LANGUAGES` に 1 要素足すだけで済む。
pub static LANGUAGES: &[Language] = &[
    Language {
        id: "rust",
        display_name: "Rust",
        extensions: &["rs"],
        filenames: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        indent_width: 4,
        use_tabs: false,
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY,
        injections_query: Some(tree_sitter_rust::INJECTIONS_QUERY),
    },
    Language {
        id: "python",
        display_name: "Python",
        extensions: &["py", "pyi", "pyw"],
        filenames: &[],
        line_comment: Some("#"),
        block_comment: None,
        indent_width: 4,
        use_tabs: false,
        grammar: || tree_sitter_python::LANGUAGE.into(),
        highlights_query: tree_sitter_python::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "javascript",
        display_name: "JavaScript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        filenames: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        highlights_query: tree_sitter_javascript::HIGHLIGHT_QUERY,
        injections_query: Some(tree_sitter_javascript::INJECTIONS_QUERY),
    },
    Language {
        id: "typescript",
        display_name: "TypeScript",
        extensions: &["ts", "mts", "cts"],
        filenames: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "tsx",
        display_name: "TypeScript JSX",
        extensions: &["tsx"],
        filenames: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "json",
        display_name: "JSON",
        extensions: &["json", "jsonc"],
        filenames: &[".prettierrc", ".babelrc"],
        line_comment: Some("//"),
        block_comment: None,
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_json::LANGUAGE.into(),
        highlights_query: tree_sitter_json::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "toml",
        display_name: "TOML",
        extensions: &["toml"],
        filenames: &["Cargo.lock"],
        line_comment: Some("#"),
        block_comment: None,
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_toml_ng::LANGUAGE.into(),
        highlights_query: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "markdown",
        display_name: "Markdown",
        extensions: &["md", "markdown", "mdx"],
        filenames: &[],
        line_comment: None,
        block_comment: Some(("<!--", "-->")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_md::LANGUAGE.into(),
        highlights_query: tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        injections_query: Some(tree_sitter_md::INJECTION_QUERY_BLOCK),
    },
    Language {
        id: "go",
        display_name: "Go",
        extensions: &["go"],
        filenames: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        indent_width: 4,
        use_tabs: true,
        grammar: || tree_sitter_go::LANGUAGE.into(),
        highlights_query: tree_sitter_go::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
    Language {
        id: "html",
        display_name: "HTML",
        extensions: &["html", "htm"],
        filenames: &[],
        line_comment: None,
        block_comment: Some(("<!--", "-->")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_html::LANGUAGE.into(),
        highlights_query: tree_sitter_html::HIGHLIGHTS_QUERY,
        injections_query: Some(tree_sitter_html::INJECTIONS_QUERY),
    },
    Language {
        id: "css",
        display_name: "CSS",
        extensions: &["css", "scss"],
        filenames: &[],
        line_comment: None,
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        use_tabs: false,
        grammar: || tree_sitter_css::LANGUAGE.into(),
        highlights_query: tree_sitter_css::HIGHLIGHTS_QUERY,
        injections_query: None,
    },
];

/// パスから言語を判定する。
pub fn detect_language(path: &Path) -> Option<&'static Language> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && let Some(lang) = LANGUAGES
            .iter()
            .find(|l| l.filenames.iter().any(|f| f.eq_ignore_ascii_case(name)))
    {
        return Some(lang);
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.extensions.iter().any(|e| *e == ext))
}

pub fn language_by_id(id: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.id == id)
}

/// コンパイル済みの文法とクエリ。
pub struct CompiledLanguage {
    pub definition: &'static Language,
    pub grammar: tree_sitter::Language,
    pub highlights: Query,
    /// クエリのキャプチャ索引 → テーマ上の意味づけ。
    ///
    /// キャプチャ名の照合を毎回やると 1 スパンごとに文字列比較が走るため、
    /// クエリのコンパイル時に一度だけ解決して表に落とす。
    pub capture_tokens: Vec<Option<TokenKind>>,
}

/// 言語ごとのコンパイル結果を貯めるキャッシュ。
///
/// プロセス内で 1 つ共有する。バックエンドは複数バッファから同時に触るため `Mutex` で包む。
#[derive(Default)]
pub struct LanguageRegistry {
    compiled: Mutex<HashMap<&'static str, Arc<CompiledLanguage>>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// プロセス全体で共有するレジストリ。
    pub fn global() -> &'static LanguageRegistry {
        static REGISTRY: OnceLock<LanguageRegistry> = OnceLock::new();
        REGISTRY.get_or_init(LanguageRegistry::new)
    }

    /// 言語をコンパイル済みの形で取得する。初回のみクエリをコンパイルする。
    pub fn get(&self, language: &'static Language) -> Result<Arc<CompiledLanguage>, QueryError> {
        let mut compiled = self.compiled.lock().expect("言語レジストリのロック");
        if let Some(existing) = compiled.get(language.id) {
            return Ok(existing.clone());
        }
        let grammar = (language.grammar)();
        let highlights = Query::new(&grammar, language.highlights_query)?;
        let capture_tokens = highlights
            .capture_names()
            .iter()
            .map(|name| token_kind_for_capture(name))
            .collect();
        let entry = Arc::new(CompiledLanguage {
            definition: language,
            grammar,
            highlights,
            capture_tokens,
        });
        compiled.insert(language.id, entry.clone());
        Ok(entry)
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Arc<CompiledLanguage>> {
        let language = detect_language(path)?;
        self.get(language).ok()
    }

    pub fn get_by_id(&self, id: &str) -> Option<Arc<CompiledLanguage>> {
        self.get(language_by_id(id)?).ok()
    }
}

/// tree-sitter のキャプチャ名をテーマの配色キーに写す。
///
/// キャプチャ名は `function.method` のようにドット区切りで細分化されるので、
/// 長い方から順に照合し、最後に先頭要素だけで再照合する。文法ごとに独自の名前が
/// 増えても、既知の接頭辞に落ちれば妥当な色がつく。
fn token_kind_for_capture(name: &str) -> Option<TokenKind> {
    if let Some(kind) = exact_token_kind(name) {
        return Some(kind);
    }
    let mut rest = name;
    while let Some(idx) = rest.rfind('.') {
        rest = &rest[..idx];
        if let Some(kind) = exact_token_kind(rest) {
            return Some(kind);
        }
    }
    None
}

fn exact_token_kind(name: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match name {
        "keyword" => Keyword,
        "keyword.control"
        | "keyword.conditional"
        | "keyword.repeat"
        | "keyword.return"
        | "keyword.exception"
        | "conditional"
        | "repeat"
        | "exception" => KeywordControl,
        "keyword.operator" => Operator,
        "function" | "function.call" | "function.method" | "function.builtin" | "method" => {
            Function
        }
        "function.macro" | "macro" | "preproc" => FunctionMacro,
        "type" | "type.builtin" | "type.definition" | "class" | "struct" | "interface" => Type,
        "constructor" => Constructor,
        "variable" | "variable.builtin" | "variable.other" => Variable,
        "variable.parameter" | "parameter" => Parameter,
        "property" | "field" | "variable.member" => Property,
        "constant" | "constant.builtin" => Constant,
        "constant.numeric" | "number" | "float" | "integer" => Number,
        "boolean" => Boolean,
        "string" | "string.special" | "character" => String,
        "escape" | "string.escape" | "string.special.symbol" => StringEscape,
        "comment" => Comment,
        "comment.documentation" | "comment.doc" => CommentDoc,
        "operator" => Operator,
        "punctuation" | "punctuation.delimiter" | "punctuation.bracket" | "punctuation.special" => {
            Punctuation
        }
        "namespace" | "module" | "package" => Namespace,
        "attribute" | "annotation" | "decorator" => Attribute,
        "tag" | "tag.builtin" => Tag,
        "label" => Label,
        "string.regexp" | "regex" => Regex,
        // Markdown の見出し (tree-sitter-md の highlights.scm が付ける名前)。
        // 本文の "text" に落としてしまうと地の文と見分けが付かなくなる。
        "text.title" | "title" => Heading,
        // コードスパン・コードブロック。文字列と同じ「そのまま読む値」という
        // 見た目にしておくと、地の文から視覚的に分離できる。
        "text.literal" => String,
        "text" | "none" | "spell" => Text,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn 拡張子から言語を判定する() {
        assert_eq!(
            detect_language(&PathBuf::from("src/main.rs")).unwrap().id,
            "rust"
        );
        assert_eq!(
            detect_language(&PathBuf::from("a/b/app.tsx")).unwrap().id,
            "tsx"
        );
        assert_eq!(
            detect_language(&PathBuf::from("Cargo.toml")).unwrap().id,
            "toml"
        );
    }

    #[test]
    fn 未対応の拡張子は_none() {
        assert!(detect_language(&PathBuf::from("photo.heic")).is_none());
        assert!(detect_language(&PathBuf::from("README")).is_none());
    }

    #[test]
    fn 拡張子の大文字小文字を無視する() {
        assert_eq!(detect_language(&PathBuf::from("A.RS")).unwrap().id, "rust");
    }

    #[test]
    fn キャプチャ名が段階的に解決される() {
        assert_eq!(token_kind_for_capture("keyword"), Some(TokenKind::Keyword));
        assert_eq!(
            token_kind_for_capture("function.method.builtin"),
            Some(TokenKind::Function),
            "未知の細分は既知の接頭辞に落ちる"
        );
        assert_eq!(
            token_kind_for_capture("variable.parameter.builtin"),
            Some(TokenKind::Parameter)
        );
        assert_eq!(token_kind_for_capture("wholly.unknown"), None);
    }

    #[test]
    fn markdownの見出しキャプチャがheadingに解決される() {
        // tree-sitter-md の highlights.scm は見出しを "text.title" で捕捉する。
        // ここが素の "text" に落ちると、見出しが地の文と同じ色になってしまう。
        assert_eq!(
            token_kind_for_capture("text.title"),
            Some(TokenKind::Heading)
        );
    }

    #[test]
    fn markdownのコードスパンはstringに解決される() {
        assert_eq!(
            token_kind_for_capture("text.literal"),
            Some(TokenKind::String)
        );
    }

    #[test]
    fn 全言語のクエリがコンパイルできる() {
        let registry = LanguageRegistry::new();
        for language in LANGUAGES {
            let compiled = registry
                .get(language)
                .unwrap_or_else(|e| panic!("{} のクエリが壊れています: {e}", language.id));
            assert!(
                compiled.capture_tokens.iter().any(Option::is_some),
                "{} のキャプチャが 1 つも対応づけられていない",
                language.id
            );
        }
    }

    #[test]
    fn 同じ言語は再コンパイルされない() {
        let registry = LanguageRegistry::new();
        let a = registry.get(&LANGUAGES[0]).unwrap();
        let b = registry.get(&LANGUAGES[0]).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn rust_では単一引用符を自動閉じしない() {
        let config = language_by_id("rust").unwrap().config();
        assert!(!config.auto_close_pairs.iter().any(|(a, _)| a == "'"));
        let config = language_by_id("python").unwrap().config();
        assert!(config.auto_close_pairs.iter().any(|(a, _)| a == "'"));
    }
}
