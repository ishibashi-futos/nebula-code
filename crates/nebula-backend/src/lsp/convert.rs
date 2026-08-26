//! lsp-types の応答を nebula-protocol の型へ翻訳する。
//!
//! GUI に LSP の型を持ち込まないための層。ここは外部プロセスにも I/O にも依存しない
//! 純粋関数だけで構成し、実際のサーバー応答を模した JSON で単体テストする。
//!
//! 「同じ文書内の範囲」を含む応答 (ホバー・補完・シンボル等) は本文を受け取って
//! その場で char 桁へ直す。一方で **他ファイルを指す結果** (定義・参照・
//! ワークスペース編集) は対象ファイルの本文が要るため、ここではパスと LSP 範囲の
//! 組に均すところまでを行い、桁変換は呼び出し側に任せる。

use super::offset;
use super::uri::uri_to_path;
use lsp_types as lsp;
use nebula_protocol::{
    CodeAction, CompletionItem, CompletionKind, Diagnostic, DiagnosticSeverity, HoverInfo,
    SignatureHelp, SignatureInfo, SymbolInfo, SymbolKind, TextEditOp,
};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// ホバー
// ---------------------------------------------------------------------------

pub fn hover(hover: lsp::Hover, text: &str) -> HoverInfo {
    HoverInfo {
        contents: hover_contents(hover.contents),
        range: hover.range.map(|r| offset::from_lsp_range(text, r)),
    }
}

/// ホバー本文を Markdown 文字列に均す。
///
/// LSP は 3 通りの表現を許しており、サーバーによってどれが来るか分からない。
/// GUI 側で場合分けさせないよう、ここで 1 つの Markdown に畳む。
fn hover_contents(contents: lsp::HoverContents) -> String {
    match contents {
        lsp::HoverContents::Scalar(marked) => marked_string(marked),
        lsp::HoverContents::Array(items) => items
            .into_iter()
            .map(marked_string)
            .collect::<Vec<_>>()
            .join("\n\n---\n\n"),
        lsp::HoverContents::Markup(markup) => markup_content(markup),
    }
}

fn marked_string(marked: lsp::MarkedString) -> String {
    match marked {
        lsp::MarkedString::String(value) => value,
        // 言語つきの断片はコードブロックに戻す。GUI は Markdown だけを解釈すればよくなる。
        lsp::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

fn markup_content(markup: lsp::MarkupContent) -> String {
    markup.value
}

fn documentation(doc: lsp::Documentation) -> String {
    match doc {
        lsp::Documentation::String(value) => value,
        lsp::Documentation::MarkupContent(markup) => markup_content(markup),
    }
}

// ---------------------------------------------------------------------------
// 補完
// ---------------------------------------------------------------------------

pub fn completions(response: lsp::CompletionResponse, text: &str) -> Vec<CompletionItem> {
    let items = match response {
        lsp::CompletionResponse::Array(items) => items,
        lsp::CompletionResponse::List(list) => list.items,
    };
    items
        .into_iter()
        .map(|item| completion_item(item, text))
        .collect()
}

fn completion_item(item: lsp::CompletionItem, text: &str) -> CompletionItem {
    // textEdit があればそれが正。無い場合だけ insertText → label と落ちる。
    let (insert_text, replace_range) = match item.text_edit {
        Some(lsp::CompletionTextEdit::Edit(edit)) => (
            Some(edit.new_text),
            Some(offset::from_lsp_range(text, edit.range)),
        ),
        // insert と replace の両方が来たときは replace を採る。
        // 既存の識別子を置き換える方が編集操作として自然なため。
        Some(lsp::CompletionTextEdit::InsertAndReplace(edit)) => (
            Some(edit.new_text),
            Some(offset::from_lsp_range(text, edit.replace)),
        ),
        None => (None, None),
    };
    let insert_text = insert_text
        .or(item.insert_text)
        .unwrap_or_else(|| item.label.clone());

    CompletionItem {
        label: item.label,
        kind: item
            .kind
            .map(completion_kind)
            .unwrap_or(CompletionKind::Text),
        detail: item.detail,
        documentation: item.documentation.map(documentation),
        insert_text,
        replace_range,
        sort_text: item.sort_text,
        filter_text: item.filter_text,
        is_snippet: item.insert_text_format == Some(lsp::InsertTextFormat::SNIPPET),
    }
}

fn completion_kind(kind: lsp::CompletionItemKind) -> CompletionKind {
    use lsp::CompletionItemKind as K;
    match kind {
        K::METHOD => CompletionKind::Method,
        K::FUNCTION => CompletionKind::Function,
        K::CONSTRUCTOR => CompletionKind::Constructor,
        K::FIELD => CompletionKind::Field,
        K::VARIABLE => CompletionKind::Variable,
        K::CLASS => CompletionKind::Class,
        K::INTERFACE => CompletionKind::Interface,
        K::MODULE => CompletionKind::Module,
        K::PROPERTY => CompletionKind::Property,
        K::UNIT => CompletionKind::Unit,
        K::VALUE => CompletionKind::Value,
        K::ENUM => CompletionKind::Enum,
        K::KEYWORD => CompletionKind::Keyword,
        K::SNIPPET => CompletionKind::Snippet,
        K::COLOR => CompletionKind::Color,
        K::FILE => CompletionKind::File,
        K::REFERENCE => CompletionKind::Reference,
        K::FOLDER => CompletionKind::Folder,
        K::ENUM_MEMBER => CompletionKind::EnumMember,
        K::CONSTANT => CompletionKind::Constant,
        K::STRUCT => CompletionKind::Struct,
        K::EVENT => CompletionKind::Event,
        K::OPERATOR => CompletionKind::Operator,
        K::TYPE_PARAMETER => CompletionKind::TypeParameter,
        _ => CompletionKind::Text,
    }
}

// ---------------------------------------------------------------------------
// 定義・参照
// ---------------------------------------------------------------------------

/// 定義ジャンプの応答を (パス, LSP 範囲) の並びに均す。
///
/// `Location` (範囲がそのまま) と `LocationLink` (`targetSelectionRange` を持つ) の
/// 2 形式があり、サーバーごとにどちらを返すか異なる。`LocationLink` では
/// **選択範囲** を採る。定義名そのものにカーソルを置きたいため。
pub fn flatten_definition(response: lsp::GotoDefinitionResponse) -> Vec<(PathBuf, lsp::Range)> {
    match response {
        lsp::GotoDefinitionResponse::Scalar(location) => {
            flatten_locations(std::iter::once(location))
        }
        lsp::GotoDefinitionResponse::Array(locations) => flatten_locations(locations),
        lsp::GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .filter_map(|link| Some((uri_to_path(&link.target_uri)?, link.target_selection_range)))
            .collect(),
    }
}

pub fn flatten_locations(
    locations: impl IntoIterator<Item = lsp::Location>,
) -> Vec<(PathBuf, lsp::Range)> {
    locations
        .into_iter()
        .filter_map(|location| Some((uri_to_path(&location.uri)?, location.range)))
        .collect()
}

// ---------------------------------------------------------------------------
// シンボル
// ---------------------------------------------------------------------------

pub fn document_symbols(response: lsp::DocumentSymbolResponse, text: &str) -> Vec<SymbolInfo> {
    match response {
        lsp::DocumentSymbolResponse::Nested(symbols) => symbols
            .into_iter()
            .map(|symbol| nested_symbol(symbol, text))
            .collect(),
        // 平坦形式は親子関係を `container_name` でしか表現できない。
        // 復元しても正確さが保証されないので、そのまま並べる。
        lsp::DocumentSymbolResponse::Flat(symbols) => symbols
            .into_iter()
            .map(|symbol| SymbolInfo {
                name: symbol.name,
                detail: symbol.container_name,
                kind: symbol_kind(symbol.kind),
                range: offset::from_lsp_range(text, symbol.location.range),
                selection_range: offset::from_lsp_range(text, symbol.location.range),
                children: Vec::new(),
            })
            .collect(),
    }
}

fn nested_symbol(symbol: lsp::DocumentSymbol, text: &str) -> SymbolInfo {
    SymbolInfo {
        name: symbol.name,
        detail: symbol.detail,
        kind: symbol_kind(symbol.kind),
        range: offset::from_lsp_range(text, symbol.range),
        selection_range: offset::from_lsp_range(text, symbol.selection_range),
        children: symbol
            .children
            .unwrap_or_default()
            .into_iter()
            .map(|child| nested_symbol(child, text))
            .collect(),
    }
}

fn symbol_kind(kind: lsp::SymbolKind) -> SymbolKind {
    use lsp::SymbolKind as K;
    match kind {
        K::MODULE => SymbolKind::Module,
        K::NAMESPACE => SymbolKind::Namespace,
        K::PACKAGE => SymbolKind::Package,
        K::CLASS => SymbolKind::Class,
        K::METHOD => SymbolKind::Method,
        K::PROPERTY => SymbolKind::Property,
        K::FIELD => SymbolKind::Field,
        K::CONSTRUCTOR => SymbolKind::Constructor,
        K::ENUM => SymbolKind::Enum,
        K::INTERFACE => SymbolKind::Interface,
        K::FUNCTION => SymbolKind::Function,
        K::VARIABLE => SymbolKind::Variable,
        K::CONSTANT => SymbolKind::Constant,
        K::STRING => SymbolKind::String,
        K::NUMBER => SymbolKind::Number,
        K::BOOLEAN => SymbolKind::Boolean,
        K::ARRAY => SymbolKind::Array,
        K::OBJECT => SymbolKind::Object,
        K::KEY => SymbolKind::Key,
        K::NULL => SymbolKind::Null,
        K::ENUM_MEMBER => SymbolKind::EnumMember,
        K::STRUCT => SymbolKind::Struct,
        K::EVENT => SymbolKind::Event,
        K::OPERATOR => SymbolKind::Operator,
        K::TYPE_PARAMETER => SymbolKind::TypeParameter,
        _ => SymbolKind::File,
    }
}

// ---------------------------------------------------------------------------
// 編集
// ---------------------------------------------------------------------------

pub fn text_edits(edits: Vec<lsp::TextEdit>, text: &str) -> Vec<TextEditOp> {
    edits
        .into_iter()
        .map(|edit| TextEditOp {
            range: offset::from_lsp_range(text, edit.range),
            new_text: edit.new_text,
        })
        .collect()
}

/// ワークスペース編集をファイル単位に均す。
///
/// `changes` と `documentChanges` のどちらで来るかはサーバー次第。
/// ファイルの作成・削除・改名操作は Nebula の [`WorkspaceEdit`] が表現できないので落とす。
///
/// [`WorkspaceEdit`]: nebula_protocol::WorkspaceEdit
pub fn flatten_workspace_edit(edit: lsp::WorkspaceEdit) -> Vec<(PathBuf, Vec<lsp::TextEdit>)> {
    let mut result: Vec<(PathBuf, Vec<lsp::TextEdit>)> = Vec::new();
    let mut push = |path: PathBuf, edits: Vec<lsp::TextEdit>| match result
        .iter_mut()
        .find(|(p, _)| *p == path)
    {
        Some((_, existing)) => existing.extend(edits),
        None => result.push((path, edits)),
    };

    if let Some(changes) = edit.changes {
        // HashMap の走査順は不定なので、GUI の表示と差分適用が安定するよう並べ替える。
        let mut entries: Vec<_> = changes
            .into_iter()
            .filter_map(|(uri, edits)| Some((uri_to_path(&uri)?, edits)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, edits) in entries {
            push(path, edits);
        }
    }

    let document_edits = match edit.document_changes {
        Some(lsp::DocumentChanges::Edits(edits)) => edits,
        Some(lsp::DocumentChanges::Operations(operations)) => operations
            .into_iter()
            .filter_map(|op| match op {
                lsp::DocumentChangeOperation::Edit(edit) => Some(edit),
                lsp::DocumentChangeOperation::Op(_) => None,
            })
            .collect(),
        None => Vec::new(),
    };
    for document_edit in document_edits {
        let Some(path) = uri_to_path(&document_edit.text_document.uri) else {
            continue;
        };
        let edits = document_edit
            .edits
            .into_iter()
            .map(|edit| match edit {
                lsp::OneOf::Left(edit) => edit,
                lsp::OneOf::Right(annotated) => annotated.text_edit,
            })
            .collect();
        push(path, edits);
    }
    result
}

// ---------------------------------------------------------------------------
// シグネチャヘルプ・コードアクション・診断
// ---------------------------------------------------------------------------

pub fn signature_help(help: lsp::SignatureHelp) -> SignatureHelp {
    SignatureHelp {
        signatures: help
            .signatures
            .into_iter()
            .map(|signature| SignatureInfo {
                label: signature.label.clone(),
                documentation: signature.documentation.map(documentation),
                parameters: signature
                    .parameters
                    .unwrap_or_default()
                    .into_iter()
                    .map(|parameter| parameter_label(parameter.label, &signature.label))
                    .collect(),
                active_parameter: signature.active_parameter.or(help.active_parameter),
            })
            .collect(),
        active_signature: help.active_signature.unwrap_or(0),
    }
}

/// 引数ラベルを文字列にする。オフセット形式はシグネチャ本文から切り出す。
fn parameter_label(label: lsp::ParameterLabel, signature: &str) -> String {
    match label {
        lsp::ParameterLabel::Simple(value) => value,
        lsp::ParameterLabel::LabelOffsets([start, end]) => {
            // オフセットは UTF-16 コードユニット単位。シグネチャは 1 行として扱う。
            let start = offset::utf16_to_char(signature, start) as usize;
            let end = offset::utf16_to_char(signature, end) as usize;
            signature
                .chars()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect()
        }
    }
}

pub fn code_actions(actions: &[lsp::CodeActionOrCommand]) -> Vec<CodeAction> {
    actions
        .iter()
        .map(|action| match action {
            lsp::CodeActionOrCommand::CodeAction(action) => CodeAction {
                title: action.title.clone(),
                kind: action.kind.as_ref().map(|k| k.as_str().to_string()),
                is_preferred: action.is_preferred.unwrap_or(false),
            },
            lsp::CodeActionOrCommand::Command(command) => CodeAction {
                title: command.title.clone(),
                kind: None,
                is_preferred: false,
            },
        })
        .collect()
}

pub fn diagnostics(diagnostics: Vec<lsp::Diagnostic>, text: &str) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .map(|diagnostic| Diagnostic {
            range: offset::from_lsp_range(text, diagnostic.range),
            // 重大度を省略するサーバーがある。最も目立つ Error に寄せて見落としを防ぐ。
            severity: diagnostic
                .severity
                .map(severity)
                .unwrap_or(DiagnosticSeverity::Error),
            message: diagnostic.message,
            source: diagnostic.source,
            code: diagnostic.code.map(|code| match code {
                lsp::NumberOrString::Number(n) => n.to_string(),
                lsp::NumberOrString::String(s) => s,
            }),
        })
        .collect()
}

fn severity(severity: lsp::DiagnosticSeverity) -> DiagnosticSeverity {
    match severity {
        lsp::DiagnosticSeverity::ERROR => DiagnosticSeverity::Error,
        lsp::DiagnosticSeverity::WARNING => DiagnosticSeverity::Warning,
        lsp::DiagnosticSeverity::INFORMATION => DiagnosticSeverity::Information,
        _ => DiagnosticSeverity::Hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::{Position, SpanRange};
    use serde_json::json;

    /// 2 行目に絵文字を置き、UTF-16 桁が char 桁とずれる状況を作る。
    const TEXT: &str = "fn main() {}\nlet s = \"🚀ロケット\";";

    fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).expect("応答の解析")
    }

    /// `uri_to_path` を通すテスト用に、一時ディレクトリ配下のファイルを指す
    /// URI 文字列とそれが復元されるべきパスの組を作る。
    ///
    /// `lsp::uri::uri_to_path` は Windows ではドライブレターの無い URI
    /// （`file:///tmp/a.rs` のような Unix 形式）を `None` にする仕様にしたため、
    /// これを経由するテストは Unix のパスをそのまま使えない。プラットフォームごとに
    /// 正しい URI とパスを 1 箇所にまとめ、各テストの分岐を無くす。
    ///
    /// `name_in_uri` は URI に埋め込む形（パーセントエンコード済みならそのまま）、
    /// `name` は復元後のパスに現れる形（デコード済み）を渡す。
    fn tmp_uri_and_path(name_in_uri: &str, name: &str) -> (String, PathBuf) {
        if cfg!(windows) {
            (
                format!("file:///C:/tmp/{name_in_uri}"),
                PathBuf::from(format!(r"C:\tmp\{name}")),
            )
        } else {
            (
                format!("file:///tmp/{name_in_uri}"),
                PathBuf::from(format!("/tmp/{name}")),
            )
        }
    }

    // -- ホバー --

    #[test]
    fn ホバーの_markupcontent_をそのまま_markdown_にする() {
        let value: lsp::Hover = parse(json!({
            "contents": { "kind": "markdown", "value": "```rust\nfn main()\n```" },
            "range": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7} }
        }));
        let info = hover(value, TEXT);
        assert_eq!(info.contents, "```rust\nfn main()\n```");
        assert_eq!(
            info.range,
            Some(SpanRange::new(Position::new(0, 3), Position::new(0, 7)))
        );
    }

    #[test]
    fn ホバーの単一文字列形式を扱える() {
        let value: lsp::Hover = parse(json!({ "contents": "ただの文字列" }));
        assert_eq!(hover(value, TEXT).contents, "ただの文字列");
        assert_eq!(hover(parse(json!({ "contents": "x" })), TEXT).range, None);
    }

    #[test]
    fn ホバーの言語つき断片はコードブロックになる() {
        let value: lsp::Hover = parse(json!({
            "contents": { "language": "rust", "value": "struct Foo" }
        }));
        assert_eq!(hover(value, TEXT).contents, "```rust\nstruct Foo\n```");
    }

    #[test]
    fn ホバーの配列形式は区切り線で連結する() {
        let value: lsp::Hover = parse(json!({
            "contents": ["一つ目", { "language": "rust", "value": "let x" }]
        }));
        assert_eq!(
            hover(value, TEXT).contents,
            "一つ目\n\n---\n\n```rust\nlet x\n```"
        );
    }

    #[test]
    fn ホバーの範囲は_utf16_から_char_桁に直る() {
        // 2 行目「let s = "🚀ロケット";」の絵文字直後は UTF-16 で 11、char で 10。
        let value: lsp::Hover = parse(json!({
            "contents": "x",
            "range": { "start": {"line": 1, "character": 9}, "end": {"line": 1, "character": 11} }
        }));
        assert_eq!(
            hover(value, TEXT).range,
            Some(SpanRange::new(Position::new(1, 9), Position::new(1, 10)))
        );
    }

    // -- 補完 --

    #[test]
    fn 補完の_textedit_から挿入範囲を取る() {
        let response: lsp::CompletionResponse = parse(json!([{
            "label": "push",
            "kind": 2,
            "detail": "fn push(&mut self, value: T)",
            "textEdit": {
                "range": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7} },
                "newText": "push($0)"
            },
            "insertTextFormat": 2
        }]));
        let items = completions(response, TEXT);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, CompletionKind::Method);
        assert_eq!(items[0].insert_text, "push($0)");
        assert!(items[0].is_snippet);
        assert_eq!(
            items[0].replace_range,
            Some(SpanRange::new(Position::new(0, 3), Position::new(0, 7)))
        );
    }

    #[test]
    fn 補完の_textedit_がなければ範囲は_none() {
        let response: lsp::CompletionResponse = parse(json!({
            "isIncomplete": false,
            "items": [{ "label": "String", "kind": 22 }]
        }));
        let items = completions(response, TEXT);
        assert_eq!(items[0].kind, CompletionKind::Struct);
        // insertText も無いときは label をそのまま挿入する。
        assert_eq!(items[0].insert_text, "String");
        assert_eq!(items[0].replace_range, None);
        assert!(!items[0].is_snippet);
    }

    #[test]
    fn 補完の_insertreplace_は置換範囲を採る() {
        let response: lsp::CompletionResponse = parse(json!([{
            "label": "value",
            "textEdit": {
                "newText": "value",
                "insert": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2} },
                "replace": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5} }
            }
        }]));
        let items = completions(response, TEXT);
        assert_eq!(
            items[0].replace_range,
            Some(SpanRange::new(Position::new(0, 0), Position::new(0, 5)))
        );
    }

    // -- 定義・参照 --

    #[test]
    fn 定義応答の_location_形式を扱える() {
        let (uri, path) = tmp_uri_and_path("a.rs", "a.rs");
        let response: lsp::GotoDefinitionResponse = parse(json!({
            "uri": uri,
            "range": { "start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 8} }
        }));
        let flat = flatten_definition(response);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].0, path);
        assert_eq!(flat[0].1.start.line, 3);
    }

    #[test]
    fn 定義応答の_locationlink_形式は選択範囲を採る() {
        let (uri, path) = tmp_uri_and_path("%E8%A8%AD%E8%A8%88.rs", "設計.rs");
        let response: lsp::GotoDefinitionResponse = parse(json!([{
            "targetUri": uri,
            "targetRange": { "start": {"line": 1, "character": 0}, "end": {"line": 9, "character": 1} },
            "targetSelectionRange": { "start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 11} }
        }]));
        let flat = flatten_definition(response);
        assert_eq!(flat[0].0, path);
        assert_eq!(flat[0].1.start.character, 7);
        assert_eq!(flat[0].1.end.character, 11);
    }

    #[test]
    fn 定義応答の配列形式を扱える() {
        let (uri_a, _path_a) = tmp_uri_and_path("a.rs", "a.rs");
        let (uri_b, path_b) = tmp_uri_and_path("b.rs", "b.rs");
        let response: lsp::GotoDefinitionResponse = parse(json!([
            { "uri": uri_a, "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} } },
            { "uri": uri_b, "range": { "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1} } }
        ]));
        let flat = flatten_definition(response);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[1].0, path_b);
    }

    // -- シンボル --

    #[test]
    fn 入れ子のシンボルを再帰的に変換する() {
        let response: lsp::DocumentSymbolResponse = parse(json!([{
            "name": "TextBuffer",
            "kind": 23,
            "range": { "start": {"line": 0, "character": 0}, "end": {"line": 20, "character": 1} },
            "selectionRange": { "start": {"line": 0, "character": 11}, "end": {"line": 0, "character": 21} },
            "children": [{
                "name": "text",
                "detail": "fn(&self) -> String",
                "kind": 6,
                "range": { "start": {"line": 2, "character": 4}, "end": {"line": 4, "character": 5} },
                "selectionRange": { "start": {"line": 2, "character": 7}, "end": {"line": 2, "character": 11} }
            }]
        }]));
        let symbols = document_symbols(response, TEXT);
        assert_eq!(symbols[0].kind, SymbolKind::Struct);
        assert_eq!(symbols[0].children[0].name, "text");
        assert_eq!(symbols[0].children[0].kind, SymbolKind::Method);
    }

    #[test]
    fn 平坦なシンボル形式も扱える() {
        let response: lsp::DocumentSymbolResponse = parse(json!([{
            "name": "main",
            "kind": 12,
            "location": {
                "uri": "file:///tmp/a.rs",
                "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 12} }
            },
            "containerName": "crate"
        }]));
        let symbols = document_symbols(response, TEXT);
        assert_eq!(symbols[0].kind, SymbolKind::Function);
        assert_eq!(symbols[0].detail.as_deref(), Some("crate"));
        assert!(symbols[0].children.is_empty());
    }

    // -- 編集 --

    #[test]
    fn ワークスペース編集の_changes_形式をパスごとに均す() {
        let (uri_a, path_a) = tmp_uri_and_path("a.rs", "a.rs");
        let (uri_b, path_b) = tmp_uri_and_path("b.rs", "b.rs");
        let edit: lsp::WorkspaceEdit = parse(json!({
            "changes": {
                uri_b: [{ "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3} }, "newText": "new" }],
                uri_a: [{ "range": { "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3} }, "newText": "new" }]
            }
        }));
        let flat = flatten_workspace_edit(edit);
        // パス順に安定して並ぶ。
        assert_eq!(flat[0].0, path_a);
        assert_eq!(flat[1].0, path_b);
    }

    #[test]
    fn ワークスペース編集の_documentchanges_形式を扱える() {
        let (uri, path) = tmp_uri_and_path("a.rs", "a.rs");
        let edit: lsp::WorkspaceEdit = parse(json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": 3 },
                "edits": [{ "range": { "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7} }, "newText": "renamed" }]
            }]
        }));
        let flat = flatten_workspace_edit(edit);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].0, path);
        let edits = text_edits(flat[0].1.clone(), TEXT);
        assert_eq!(edits[0].new_text, "renamed");
        assert_eq!(
            edits[0].range,
            SpanRange::new(Position::new(0, 3), Position::new(0, 7))
        );
    }

    #[test]
    fn ファイル操作を含む_documentchanges_は編集だけ拾う() {
        let (uri_new, _path_new) = tmp_uri_and_path("new.rs", "new.rs");
        let (uri_a, path_a) = tmp_uri_and_path("a.rs", "a.rs");
        let edit: lsp::WorkspaceEdit = parse(json!({
            "documentChanges": [
                { "kind": "create", "uri": uri_new },
                {
                    "textDocument": { "uri": uri_a, "version": null },
                    "edits": [{ "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} }, "newText": "x" }]
                }
            ]
        }));
        let flat = flatten_workspace_edit(edit);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].0, path_a);
    }

    // -- シグネチャヘルプ --

    #[test]
    fn シグネチャヘルプの引数ラベルを文字列にする() {
        let help: lsp::SignatureHelp = parse(json!({
            "signatures": [{
                "label": "fn push(&mut self, value: T)",
                "documentation": "末尾に追加する",
                "parameters": [
                    { "label": "&mut self" },
                    { "label": [19, 27] }
                ],
                "activeParameter": 1
            }],
            "activeSignature": 0
        }));
        let converted = signature_help(help);
        assert_eq!(converted.active_signature, 0);
        assert_eq!(converted.signatures[0].parameters[0], "&mut self");
        assert_eq!(converted.signatures[0].parameters[1], "value: T");
        assert_eq!(converted.signatures[0].active_parameter, Some(1));
        assert_eq!(
            converted.signatures[0].documentation.as_deref(),
            Some("末尾に追加する")
        );
    }

    // -- コードアクション・診断 --

    #[test]
    fn コードアクションとコマンドを一覧にできる() {
        let actions: lsp::CodeActionResponse = parse(json!([
            { "title": "use std::fmt", "kind": "quickfix", "isPreferred": true },
            { "title": "Run test", "command": "rust-analyzer.runSingle" }
        ]));
        let converted = code_actions(&actions);
        assert_eq!(converted[0].kind.as_deref(), Some("quickfix"));
        assert!(converted[0].is_preferred);
        assert_eq!(converted[1].title, "Run test");
        assert_eq!(converted[1].kind, None);
    }

    #[test]
    fn 診断を変換し重大度省略時は_error_にする() {
        let raw: Vec<lsp::Diagnostic> = parse(json!([
            {
                "range": { "start": {"line": 1, "character": 9}, "end": {"line": 1, "character": 11} },
                "severity": 2,
                "message": "未使用の変数",
                "source": "rustc",
                "code": "unused_variables"
            },
            {
                "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2} },
                "message": "重大度なし",
                "code": 42
            }
        ]));
        let converted = diagnostics(raw, TEXT);
        assert_eq!(converted[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(converted[0].source.as_deref(), Some("rustc"));
        // 絵文字を含む行なので UTF-16 桁 11 は char 桁 10 になる。
        assert_eq!(converted[0].range.end, Position::new(1, 10));
        assert_eq!(converted[1].severity, DiagnosticSeverity::Error);
        assert_eq!(converted[1].code.as_deref(), Some("42"));
    }
}
