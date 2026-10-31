//! Phase 4 spike: full-screen TUI on Mojave over SSH. Prints a report line to stderr on exit.
use crossterm::{event::{self, Event, KeyCode, KeyModifiers, EnableBracketedPaste, DisableBracketedPaste},
    execute, terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}};
use ratatui::{prelude::*, widgets::*};
use std::{io, time::Duration};

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let mut log: Vec<String> = vec![];
    let mut input = String::new();
    let mut frames = 0u32;
    let res = loop {
        term.draw(|f| {
            let a = f.area();
            let c = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(a);
            let items: Vec<ListItem> = log.iter().map(|l| ListItem::new(l.as_str())).collect();
            f.render_widget(List::new(items).block(Block::bordered().title(format!(" moco spike {}x{} ", a.width, a.height))
                .border_style(Style::new().fg(Color::Cyan))), c[0]);
            f.render_widget(Paragraph::new(input.as_str()).block(Block::bordered().title(" prompt (Ctrl-D quits) ")), c[1]);
            frames += 1;
        })?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(k) => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('d') | KeyCode::Char('c')) { break Ok(()); }
                    match k.code {
                        KeyCode::Char(ch) => input.push(ch),
                        KeyCode::Backspace => { input.pop(); }
                        KeyCode::Enter => { log.push(format!("> {input}  (key event: {:?})", k.code)); input.clear(); }
                        other => log.push(format!("key {other:?} {:?}", k.modifiers)),
                    }
                }
                Event::Paste(s) => log.push(format!("paste {} bytes", s.len())),
                Event::Resize(w, h) => log.push(format!("resize {w}x{h}")),
                _ => {}
            }
        }
    };
    disable_raw_mode()?;
    execute!(term.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen)?;
    eprintln!("frames={frames} log_lines={}", log.len());
    for l in &log { eprintln!("{l}"); }
    res
}
