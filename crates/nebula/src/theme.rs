//! Cyber-Cosmic テーマ。
//!
//! 「宇宙空間に浮かぶ計器盤」を狙った配色。背景は完全な黒ではなく、わずかに青紫へ寄せた
//! 暗色 (深宇宙) を使う。真っ黒だと有機 EL 以外のディスプレイで沈みすぎ、
//! ネオン色との境界がにじんで見えるため。
//!
//! アクセントは 3 色に絞る。シアン (主), マゼンタ (副), バイオレット (装飾)。
//! ネオンを多用すると視線の優先順位が壊れるので、面積の大きい要素には使わない。

use gpui::{App, Global, Hsla, rgb, rgba};
use nebula_protocol::{DiagnosticSeverity, TermColor, TokenKind};

/// 配色の一式。
#[derive(Debug, Clone)]
pub struct Theme {
    // -- 面 --
    /// ウィンドウ最背面。星空にあたる最も暗い層。
    pub bg_void: Hsla,
    /// 主要な面 (エディタ本体)。
    pub bg_surface: Hsla,
    /// 一段持ち上がった面 (サイドバー・パネル)。
    pub bg_elevated: Hsla,
    /// さらに持ち上がった面 (ポップアップ・コマンドパレット)。
    pub bg_overlay: Hsla,
    /// アクティビティバー (最左の縦帯)。
    pub bg_activity: Hsla,

    // -- 線 --
    pub border: Hsla,
    pub border_strong: Hsla,
    /// ネオンの縁取り。フォーカス中の要素に使う。
    pub border_glow: Hsla,

    // -- 文字 --
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_inverse: Hsla,

    // -- アクセント --
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub accent_secondary: Hsla,
    pub accent_tertiary: Hsla,

    // -- 状態 --
    pub error: Hsla,
    pub warning: Hsla,
    pub info: Hsla,
    pub success: Hsla,

    // -- エディタ --
    pub editor_bg: Hsla,
    pub editor_gutter: Hsla,
    pub line_number: Hsla,
    pub line_number_active: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    /// 選択していないときの同一語ハイライト。
    pub selection_match: Hsla,
    pub current_line: Hsla,
    pub indent_guide: Hsla,
    pub indent_guide_active: Hsla,
    pub bracket_match: Hsla,

    // -- git --
    pub git_added: Hsla,
    pub git_modified: Hsla,
    pub git_deleted: Hsla,
    pub git_conflict: Hsla,
    pub git_ignored: Hsla,

    // -- 構文 --
    syntax: [Hsla; TOKEN_KIND_COUNT],

    // -- 端末 ANSI 16 色 --
    ansi: [Hsla; 16],
}

const TOKEN_KIND_COUNT: usize = 24;

/// [`TokenKind`] を配列添字に写す。
///
/// `TokenKind` に `usize` への変換を持たせず外側で写すのは、プロトコル型を
/// 描画都合から独立させておくため。
fn token_index(token: TokenKind) -> usize {
    use TokenKind::*;
    match token {
        Keyword => 0,
        KeywordControl => 1,
        Function => 2,
        FunctionMacro => 3,
        Type => 4,
        Constructor => 5,
        Variable => 6,
        Parameter => 7,
        Property => 8,
        Constant => 9,
        String => 10,
        StringEscape => 11,
        Number => 12,
        Boolean => 13,
        Comment => 14,
        CommentDoc => 15,
        Operator => 16,
        Punctuation => 17,
        Namespace => 18,
        Attribute => 19,
        Tag => 20,
        Label => 21,
        Regex => 22,
        Text => 23,
    }
}

impl Theme {
    pub fn cyber_cosmic() -> Self {
        // 星雲の色相をまとめて定義しておき、以下ではこれらの濃淡だけを使う。
        let cyan = rgb(0x22E6FF).into();
        let magenta = rgb(0xFF2FD0).into();
        let violet = rgb(0x9B6BFF).into();
        let lime = rgb(0x7CFF9B).into();
        let amber = rgb(0xFFC46B).into();
        let coral = rgb(0xFF6B81).into();

        let syntax = {
            let mut s = [rgb(0xE6EAFF).into(); TOKEN_KIND_COUNT];
            s[token_index(TokenKind::Keyword)] = magenta;
            s[token_index(TokenKind::KeywordControl)] = rgb(0xFF6BE0).into();
            s[token_index(TokenKind::Function)] = cyan;
            s[token_index(TokenKind::FunctionMacro)] = rgb(0x6BFFE0).into();
            s[token_index(TokenKind::Type)] = rgb(0xFFD98A).into();
            s[token_index(TokenKind::Constructor)] = rgb(0xFFD98A).into();
            s[token_index(TokenKind::Variable)] = rgb(0xD5DCFF).into();
            s[token_index(TokenKind::Parameter)] = rgb(0xB8C4FF).into();
            s[token_index(TokenKind::Property)] = rgb(0x8FD9FF).into();
            s[token_index(TokenKind::Constant)] = violet;
            s[token_index(TokenKind::String)] = lime;
            s[token_index(TokenKind::StringEscape)] = amber;
            s[token_index(TokenKind::Number)] = amber;
            s[token_index(TokenKind::Boolean)] = violet;
            // コメントは彩度を落とした青灰。目に入るが読み飛ばせる明度に置く。
            s[token_index(TokenKind::Comment)] = rgb(0x5C6690).into();
            s[token_index(TokenKind::CommentDoc)] = rgb(0x6E7BB0).into();
            s[token_index(TokenKind::Operator)] = rgb(0xFF9BE8).into();
            s[token_index(TokenKind::Punctuation)] = rgb(0x8A93B8).into();
            s[token_index(TokenKind::Namespace)] = rgb(0x9FE8FF).into();
            s[token_index(TokenKind::Attribute)] = coral;
            s[token_index(TokenKind::Tag)] = magenta;
            s[token_index(TokenKind::Label)] = coral;
            s[token_index(TokenKind::Regex)] = rgb(0xFFB86B).into();
            s[token_index(TokenKind::Text)] = rgb(0xE6EAFF).into();
            s
        };

        let ansi = [
            rgb(0x1A1E33).into(), // black
            rgb(0xFF6B81).into(), // red
            rgb(0x7CFF9B).into(), // green
            rgb(0xFFC46B).into(), // yellow
            rgb(0x6BA8FF).into(), // blue
            rgb(0xFF2FD0).into(), // magenta
            rgb(0x22E6FF).into(), // cyan
            rgb(0xC8CFEE).into(), // white
            rgb(0x39406B).into(), // bright black
            rgb(0xFF93A3).into(), // bright red
            rgb(0xA8FFBF).into(), // bright green
            rgb(0xFFD98A).into(), // bright yellow
            rgb(0x93C4FF).into(), // bright blue
            rgb(0xFF6BE0).into(), // bright magenta
            rgb(0x7FF0FF).into(), // bright cyan
            rgb(0xF2F5FF).into(), // bright white
        ];

        Self {
            bg_void: rgb(0x05060D).into(),
            bg_surface: rgb(0x0A0C16).into(),
            bg_elevated: rgb(0x0F1220).into(),
            bg_overlay: rgb(0x151A2E).into(),
            bg_activity: rgb(0x07080F).into(),

            border: rgba(0x2A3358AA).into(),
            border_strong: rgb(0x3A4470).into(),
            border_glow: rgba(0x22E6FF66).into(),

            text: rgb(0xE6EAFF).into(),
            text_muted: rgb(0x9AA3C8).into(),
            text_faint: rgb(0x5C6690).into(),
            text_inverse: rgb(0x05060D).into(),

            accent: cyan,
            accent_soft: rgba(0x22E6FF22).into(),
            accent_secondary: magenta,
            accent_tertiary: violet,

            error: coral,
            warning: amber,
            info: cyan,
            success: lime,

            editor_bg: rgb(0x0A0C16).into(),
            editor_gutter: rgb(0x0A0C16).into(),
            line_number: rgb(0x39406B).into(),
            line_number_active: cyan,
            cursor: cyan,
            selection: rgba(0x22E6FF2E).into(),
            selection_match: rgba(0x9B6BFF2E).into(),
            current_line: rgba(0xFFFFFF08).into(),
            indent_guide: rgba(0x2A335866).into(),
            indent_guide_active: rgba(0x22E6FF55).into(),
            bracket_match: rgba(0xFF2FD055).into(),

            git_added: lime,
            git_modified: amber,
            git_deleted: coral,
            git_conflict: magenta,
            git_ignored: rgb(0x4A5378).into(),

            syntax,
            ansi,
        }
    }

    pub fn syntax_color(&self, token: TokenKind) -> Hsla {
        self.syntax[token_index(token)]
    }

    pub fn diagnostic_color(&self, severity: DiagnosticSeverity) -> Hsla {
        match severity {
            DiagnosticSeverity::Error => self.error,
            DiagnosticSeverity::Warning => self.warning,
            DiagnosticSeverity::Information => self.info,
            DiagnosticSeverity::Hint => self.text_muted,
        }
    }

    /// 端末セルの色を解決する。
    pub fn term_color(&self, color: TermColor, is_background: bool) -> Hsla {
        match color {
            TermColor::Default => {
                if is_background {
                    self.bg_surface
                } else {
                    self.text
                }
            }
            TermColor::Indexed(i) => self.ansi_color(i),
            TermColor::Rgb(r, g, b) => {
                gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
            }
        }
    }

    /// 256 色パレットを解決する。
    ///
    /// 0-15 はテーマの 16 色、16-231 は 6x6x6 のカラーキューブ、232-255 はグレースケール。
    /// 後者 2 つは xterm の定義通りに計算する。
    fn ansi_color(&self, index: u8) -> Hsla {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let i = index - 16;
                let steps = [0u32, 95, 135, 175, 215, 255];
                let r = steps[(i / 36) as usize];
                let g = steps[((i % 36) / 6) as usize];
                let b = steps[(i % 6) as usize];
                gpui::rgb((r << 16) | (g << 8) | b).into()
            }
            232..=255 => {
                let level = 8 + (index as u32 - 232) * 10;
                gpui::rgb((level << 16) | (level << 8) | level).into()
            }
        }
    }
}

impl Global for Theme {}

/// 現在のテーマを取り出す。
pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

pub fn init(cx: &mut App) {
    cx.set_global(Theme::cyber_cosmic());
}

/// UI 全体で使う寸法。1 か所に集めて、画面ごとの数値のばらつきを防ぐ。
pub mod metrics {
    use gpui::{Pixels, px};

    pub const ACTIVITY_BAR_WIDTH: Pixels = px(52.);
    pub const SIDEBAR_MIN_WIDTH: Pixels = px(180.);
    pub const SIDEBAR_DEFAULT_WIDTH: Pixels = px(260.);
    pub const SIDEBAR_MAX_WIDTH: Pixels = px(600.);
    pub const TAB_HEIGHT: Pixels = px(36.);
    pub const STATUS_BAR_HEIGHT: Pixels = px(24.);
    pub const PANEL_DEFAULT_HEIGHT: Pixels = px(260.);
    pub const PANEL_MIN_HEIGHT: Pixels = px(80.);
    /// エディタ行番号ガターの幅の下限。
    pub const GUTTER_MIN_WIDTH: Pixels = px(52.);
    pub const EDITOR_FONT_SIZE: Pixels = px(13.);
    pub const UI_FONT_SIZE: Pixels = px(12.5);
    pub const LINE_HEIGHT_RATIO: f32 = 1.55;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全トークン種別に色が割り当てられている() {
        let theme = Theme::cyber_cosmic();
        // token_index が全種別で相異なる添字を返すことを確認する。
        // 添字が衝突していると別種のトークンが同じ色になり、原因追跡が難しいバグになる。
        let kinds = [
            TokenKind::Keyword,
            TokenKind::KeywordControl,
            TokenKind::Function,
            TokenKind::FunctionMacro,
            TokenKind::Type,
            TokenKind::Constructor,
            TokenKind::Variable,
            TokenKind::Parameter,
            TokenKind::Property,
            TokenKind::Constant,
            TokenKind::String,
            TokenKind::StringEscape,
            TokenKind::Number,
            TokenKind::Boolean,
            TokenKind::Comment,
            TokenKind::CommentDoc,
            TokenKind::Operator,
            TokenKind::Punctuation,
            TokenKind::Namespace,
            TokenKind::Attribute,
            TokenKind::Tag,
            TokenKind::Label,
            TokenKind::Regex,
            TokenKind::Text,
        ];
        assert_eq!(kinds.len(), TOKEN_KIND_COUNT);
        let mut indices: Vec<usize> = kinds.iter().map(|k| token_index(*k)).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), TOKEN_KIND_COUNT, "添字が衝突している");
        for kind in kinds {
            let _ = theme.syntax_color(kind);
        }
    }

    #[test]
    fn ansi_カラーキューブが_xterm_の定義に一致する() {
        let theme = Theme::cyber_cosmic();
        // 16 は (0,0,0)、231 は (255,255,255)。
        let black: gpui::Rgba = theme.ansi_color(16).into();
        assert!(black.r < 0.01 && black.g < 0.01 && black.b < 0.01);
        let white: gpui::Rgba = theme.ansi_color(231).into();
        assert!(white.r > 0.99 && white.g > 0.99 && white.b > 0.99);
    }

    #[test]
    fn グレースケール領域が単調に明るくなる() {
        let theme = Theme::cyber_cosmic();
        let first: gpui::Rgba = theme.ansi_color(232).into();
        let last: gpui::Rgba = theme.ansi_color(255).into();
        assert!(first.r < last.r);
    }

    #[test]
    fn 既定色は前景と背景で異なる() {
        let theme = Theme::cyber_cosmic();
        assert_ne!(
            theme.term_color(TermColor::Default, true),
            theme.term_color(TermColor::Default, false)
        );
    }
}
