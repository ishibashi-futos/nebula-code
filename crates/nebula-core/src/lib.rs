//! Nebula Code のテキストコア。
//!
//! GUI プロセスとバックエンドプロセスが共有する、UI にもトランスポートにも依存しない層。
//! Rope バッファ、カーソル操作、Tree-sitter による構文解析だけを持つ。

pub mod buffer;
pub mod error;
pub mod language;
pub mod markdown;
pub mod rope_ext;
pub mod selection;
pub mod syntax;

pub use buffer::{BytePoint, EditRecord, TextBuffer};
pub use error::{CoreError, Result};
pub use language::{Language, LanguageRegistry};
pub use markdown::{ListContinuation, list_continuation};
pub use rope_ext::{RopeExt, matching_bracket};
pub use selection::{Direction, Movement, Selection, move_selection, normalize};
pub use syntax::SyntaxTree;
