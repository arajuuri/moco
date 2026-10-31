//! The symbols the UI draws with. Unicode by default (box drawing, bullets and check marks that
//! Menlo and every other terminal font has); `tui.ascii` or `MOCO_ASCII=1` switches to plain ASCII.
use ratatui::symbols::border::{self, Set};

#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub ascii: bool,
    pub border: Set<'static>,
    pub user_bar: &'static str,
    pub quote_bar: &'static str,
    pub bullet: &'static str,
    pub rule: &'static str,
    pub prompt: &'static str,
    pub ok: &'static str,
    pub fail: &'static str,
    pub run: &'static str,
    pub idle: &'static str,
    pub done: &'static str,
    pub active: &'static str,
    pub gauge_full: &'static str,
    pub gauge_empty: &'static str,
    pub next: &'static str,
    pub select: &'static str,
    pub ellipsis: &'static str,
    pub sep: &'static str,
    pub vbar: &'static str,
    pub cross: &'static str,
    pub think: &'static str,
}

pub const UNICODE: Glyphs = Glyphs {
    ascii: false,
    border: border::ROUNDED,
    user_bar: "▌",
    quote_bar: "▎",
    bullet: "•",
    rule: "─",
    prompt: "›",
    ok: "✓",
    fail: "✗",
    run: "●",
    idle: "○",
    done: "●",
    active: "◐",
    gauge_full: "━",
    gauge_empty: "─",
    next: "↳",
    select: "▸",
    ellipsis: "…",
    sep: "·",
    vbar: "│",
    cross: "┼",
    think: "∴",
};

pub const ASCII: Glyphs = Glyphs {
    ascii: true,
    border: Set { top_left: "+", top_right: "+", bottom_left: "+", bottom_right: "+", vertical_left: "|", vertical_right: "|", horizontal_top: "-", horizontal_bottom: "-" },
    user_bar: ">",
    quote_bar: "|",
    bullet: "*",
    rule: "-",
    prompt: ">",
    ok: "+",
    fail: "x",
    run: "*",
    idle: "o",
    done: "*",
    active: "~",
    gauge_full: "#",
    gauge_empty: "-",
    next: "->",
    select: ">",
    ellipsis: "...",
    sep: "|",
    vbar: "|",
    cross: "+",
    think: "~",
};

pub fn choose(ascii: bool) -> &'static Glyphs {
    if ascii || std::env::var("MOCO_ASCII").is_ok_and(|v| !v.is_empty() && v != "0") || std::env::var("TERM").is_ok_and(|t| t == "linux" || t == "dumb") {
        &ASCII
    } else {
        &UNICODE
    }
}
