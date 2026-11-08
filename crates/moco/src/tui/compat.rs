//! ratatui 0.26+ conveniences back-ported onto ratatui 0.25, the last release that builds with Rust 1.73 (the last
//! toolchain that targets macOS 10.7). Call sites stay as on `main`; bring them into scope with `use super::compat::*;`.
#![allow(dead_code)]
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::block::{Position, Title};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

/// `Frame::area` (0.28) and `Frame::set_cursor_position` (0.28).
pub trait FrameCompat {
    fn area(&self) -> Rect;
    fn set_cursor_position(&mut self, pos: (u16, u16));
}

impl FrameCompat for Frame<'_> {
    fn area(&self) -> Rect {
        self.size()
    }
    fn set_cursor_position(&mut self, (x, y): (u16, u16)) {
        self.set_cursor(x, y)
    }
}

/// `Layout::vertical` / `Layout::horizontal` (0.26).
pub trait LayoutCompat {
    fn vertical<I: IntoIterator<Item = Constraint>>(constraints: I) -> Layout;
    fn horizontal<I: IntoIterator<Item = Constraint>>(constraints: I) -> Layout;
}

impl LayoutCompat for Layout {
    fn vertical<I: IntoIterator<Item = Constraint>>(constraints: I) -> Layout {
        Layout::default().direction(Direction::Vertical).constraints(constraints.into_iter().collect::<Vec<_>>())
    }
    fn horizontal<I: IntoIterator<Item = Constraint>>(constraints: I) -> Layout {
        Layout::default().direction(Direction::Horizontal).constraints(constraints.into_iter().collect::<Vec<_>>())
    }
}

/// `Block::bordered` (0.26) and `Block::title_bottom` (0.26).
pub trait BlockCompat<'a> {
    fn bordered() -> Block<'a>;
    fn title_bottom<T: Into<Line<'a>>>(self, title: T) -> Block<'a>;
}

impl<'a> BlockCompat<'a> for Block<'a> {
    fn bordered() -> Block<'a> {
        Block::default().borders(Borders::ALL)
    }
    fn title_bottom<T: Into<Line<'a>>>(self, title: T) -> Block<'a> {
        // 0.25 titles ignore the line's own alignment, so carry it over.
        let line: Line<'a> = title.into();
        let align = line.alignment;
        let mut t = Title::from(line).position(Position::Bottom);
        if let Some(a) = align {
            t = t.alignment(a);
        }
        self.title(t)
    }
}

/// `Line::style` (0.26). 0.25 lines carry no style of their own, so it is patched under each span's style, which renders
/// the same wherever the line's background is also set by the surrounding widget (the status bar and footer do this).
pub trait LineCompat {
    fn style(self, style: Style) -> Self;
}

impl LineCompat for Line<'_> {
    fn style(mut self, style: Style) -> Self {
        for s in &mut self.spans {
            s.style = style.patch(s.style);
        }
        self
    }
}
