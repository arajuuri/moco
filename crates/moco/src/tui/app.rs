//! The TUI model: state, the pure `update` function and `render`. No terminal or engine in here,
//! so tests drive it with scripted events and read ratatui's test buffer.
use crate::human;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use moco_core::engine::Reply;
use moco_core::provider::Usage;
use moco_core::tools::Todo;
use ratatui::{prelude::*, widgets::*};
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use unicode_width::UnicodeWidthChar;

/// Semantic colors. Named ANSI colors only, so Terminal.app (256 colors) and light/dark profiles work.
pub struct Theme {
    pub user: Style,
    pub text: Style,
    pub dim: Style,
    pub error: Style,
    pub accent: Style,
    pub bar: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            user: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            text: Style::new(),
            dim: Style::new().add_modifier(Modifier::DIM),
            error: Style::new().fg(Color::Red),
            accent: Style::new().fg(Color::Cyan),
            bar: Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// An engine event converted to owned data (the engine's `UiEvent` borrows).
#[derive(Debug, Clone)]
pub enum Ev {
    Text(String),
    Tool { name: String, summary: String },
    ToolDone { name: String, ok: bool, msg: String },
    Retry { attempt: u32, secs: u64, error: String },
    Usage { usage: Usage, cost: Option<f64>, session_cost: f64, context: u64, limit: Option<u64> },
    Notice(String),
    Status(String),
    Todos(Vec<Todo>),
    Titled(String),
}

pub enum AppEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
    Tick,
    /// `depth` is 0 for the session itself and grows with nested subagents.
    Engine { depth: usize, agent: String, ev: Ev },
    Ask { action: String, resources: Vec<String>, save: Vec<String>, reply: Sender<Reply> },
    /// A line from the engine's notifier (background job finished, ...).
    Background(String),
    RunDone(Result<moco_core::engine::RunOutcome, String>),
    /// A `!command` the user ran finished.
    ShellDone(Result<String, String>),
}

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Replaces the typed token (a trailing space is added unless it ends in `/`).
    pub insert: String,
    pub label: String,
    pub detail: String,
}

pub struct Popup {
    pub items: Vec<Item>,
    pub sel: usize,
    /// Char range of the token being completed.
    pub start: usize,
    pub token: String,
}

const MAX_POPUP: usize = 8;
const MAX_COMPOSER_LINES: usize = 8;
const PASTE_LINES: usize = 6;
const PASTE_CHARS: usize = 800;

/// What the event loop must do after an update.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// `text` has pasted blocks expanded; `shown` is what the transcript displays.
    Submit { text: String, shown: String },
    /// Enter on an empty composer: let the assistant react to queued background results.
    Poke,
    Interrupt,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Done,
    Failed(String),
}

pub enum Row {
    User(String),
    Assistant(String),
    Tool { depth: usize, agent: String, name: String, summary: String, state: ToolState },
    Todos(Vec<Todo>),
    Info(String),
    Shell(String),
    Error(String),
    Usage(String),
}

pub struct Permission {
    pub action: String,
    pub resources: Vec<String>,
    pub save: Vec<String>,
    pub reply: Sender<Reply>,
    /// Some while the user is typing feedback for a rejection.
    pub feedback: Option<String>,
}

#[derive(Default)]
struct RunUsage {
    usage: Usage,
    cost: f64,
    priced: bool,
    steps: u32,
}

pub struct App {
    pub rows: Vec<Row>,
    pub input: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub running: bool,
    /// Lines scrolled up from the bottom; 0 follows the newest output.
    pub scroll: usize,
    pub dialogs: VecDeque<Permission>,
    pub model: String,
    pub agent: String,
    pub title: Option<String>,
    pub session_cost: f64,
    pub context: u64,
    pub limit: Option<u64>,
    pub hint: Option<String>,
    pub tick: u64,
    pub history: Vec<String>,
    hist: Option<usize>,
    draft: String,
    pastes: Vec<(String, String)>,
    pub popup: Option<Popup>,
    dismissed: Option<String>,
    pub completer: Option<Box<dyn Fn(&str) -> Vec<Item>>>,
    /// Inner width of the composer, set by `render` (for Up/Down over wrapped lines).
    comp_w: usize,
    /// Column to aim for while moving up and down over lines of different lengths.
    goal_col: Option<usize>,
    pub view_h: usize,
    pub theme: Theme,
    run: RunUsage,
}

impl App {
    pub fn new(model: &str, agent: &str) -> App {
        App {
            rows: vec![],
            input: String::new(),
            cursor: 0,
            running: false,
            scroll: 0,
            dialogs: VecDeque::new(),
            model: model.to_string(),
            agent: agent.to_string(),
            title: None,
            session_cost: 0.0,
            context: 0,
            limit: None,
            hint: None,
            tick: 0,
            history: vec![],
            hist: None,
            draft: String::new(),
            pastes: vec![],
            popup: None,
            dismissed: None,
            completer: None,
            comp_w: 80,
            goal_col: None,
            view_h: 20,
            theme: Theme::default(),
            run: RunUsage::default(),
        }
    }

    pub fn push_info(&mut self, s: impl Into<String>) {
        self.rows.push(Row::Info(s.into()));
    }

    pub fn push_user(&mut self, s: impl Into<String>) {
        self.rows.push(Row::User(s.into()));
        self.scroll = 0;
    }

    pub fn update(&mut self, ev: AppEvent) -> Action {
        match ev {
            AppEvent::Key(k) => return self.key(k),
            AppEvent::Paste(s) => {
                if let Some(Permission { feedback: Some(f), .. }) = self.dialogs.front_mut() {
                    f.push_str(&s.replace(['\r', '\n'], " "));
                } else {
                    self.paste(&s);
                    self.refresh_popup();
                }
            }
            AppEvent::Resize => {}
            AppEvent::Tick => self.tick += 1,
            AppEvent::Engine { depth, agent, ev } => self.engine(depth, agent, ev),
            AppEvent::Ask { action, resources, save, reply } => {
                self.dialogs.push_back(Permission { action, resources, save, reply, feedback: None });
                self.scroll = 0;
            }
            AppEvent::Background(line) => self.push_info(format!("[{line}]")),
            AppEvent::ShellDone(r) => {
                self.running = false;
                match r {
                    Ok(out) => self.rows.push(Row::Shell(out)),
                    Err(e) => self.rows.push(Row::Error(e)),
                }
            }
            AppEvent::RunDone(outcome) => {
                use moco_core::engine::RunOutcome::*;
                self.running = false;
                self.dialogs.clear(); // a dialog whose run is gone cannot be answered
                match outcome {
                    Ok(Idle) | Ok(Failed(_)) => {} // failures were reported as notices
                    Ok(Interrupted) => self.push_info("interrupted"),
                    Ok(Declined) => self.push_info("stopped: tool call rejected"),
                    Ok(Busy) => self.rows.push(Row::Error("session is busy".into())),
                    Err(e) => self.rows.push(Row::Error(format!("store error: {e}"))),
                }
                self.finish_usage();
            }
        }
        Action::None
    }

    fn finish_usage(&mut self) {
        let r = std::mem::take(&mut self.run);
        if r.steps == 0 {
            return;
        }
        let cost = if r.priced { format!("${:.4} (session ${:.4})", r.cost, self.session_cost) } else { "cost n/a".to_string() };
        let ctx = match self.limit {
            Some(l) if l > 0 => format!("context {}% of {}", (self.context * 100 / l).min(100), human(l)),
            _ => format!("context {}", human(self.context)),
        };
        let wrote = if r.usage.cache_write > 0 { format!(", {} cache written", human(r.usage.cache_write)) } else { String::new() };
        self.rows.push(Row::Usage(format!("{} in, {} out, {} cached{wrote} | {cost} | {ctx}", human(r.usage.input), human(r.usage.output), human(r.usage.cache_read))));
    }

    fn engine(&mut self, depth: usize, agent: String, ev: Ev) {
        match ev {
            Ev::Text(t) => {
                if depth > 0 {
                    return; // subagent prose stays hidden, as in the plain chat
                }
                match self.rows.last_mut() {
                    Some(Row::Assistant(s)) => s.push_str(&t),
                    _ => self.rows.push(Row::Assistant(t)),
                }
            }
            Ev::Tool { name, summary } => self.rows.push(Row::Tool { depth, agent, name, summary, state: ToolState::Running }),
            Ev::ToolDone { name, ok, msg } => {
                let found = self.rows.iter_mut().rev().find(|r| matches!(r, Row::Tool { depth: d, name: n, state: ToolState::Running, .. } if *d == depth && *n == name));
                if let Some(Row::Tool { state, .. }) = found {
                    *state = if ok { ToolState::Done } else { ToolState::Failed(msg) };
                }
            }
            Ev::Retry { attempt, secs, error } => self.push_info(format!("retrying in {secs}s (attempt {attempt}): {error}")),
            Ev::Usage { usage, cost, session_cost, context, limit } => {
                self.run.usage.input += usage.input;
                self.run.usage.output += usage.output;
                self.run.usage.cache_read += usage.cache_read;
                self.run.usage.cache_write += usage.cache_write;
                self.run.cost += cost.unwrap_or(0.0);
                self.run.priced |= cost.is_some();
                self.run.steps += 1;
                if depth == 0 {
                    self.session_cost = session_cost;
                    self.context = context;
                    self.limit = limit;
                }
            }
            Ev::Notice(n) => self.rows.push(Row::Error(if depth > 0 { format!("[{agent}] {n}") } else { n })),
            Ev::Status(s) => {
                if depth == 0 {
                    self.push_info(s)
                }
            }
            Ev::Todos(t) => {
                if depth == 0 {
                    self.rows.push(Row::Todos(t))
                }
            }
            Ev::Titled(t) => {
                if depth == 0 {
                    self.title = Some(t)
                }
            }
        }
    }

    // ---- composer ----

    fn byte_at(&self, chars: usize) -> usize {
        self.input.char_indices().nth(chars).map_or(self.input.len(), |(i, _)| i)
    }

    fn len_chars(&self) -> usize {
        self.input.chars().count()
    }

    fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control() || *c == '\n').collect();
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, &s);
        self.cursor += s.chars().count();
    }

    fn delete_range(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte_at(from), self.byte_at(to));
        self.input.replace_range(a..b, "");
        self.cursor = from;
    }

    /// Start and end of the logical line (between line breaks) the cursor is in.
    fn line_start(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        (0..self.cursor).rev().find(|&i| chars[i] == '\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        (self.cursor..chars.len()).find(|&i| chars[i] == '\n').unwrap_or(chars.len())
    }

    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Pasted text: short pastes go in as typed (newlines kept); big ones become a placeholder that
    /// is expanded again on send, so the composer stays readable.
    fn paste(&mut self, raw: &str) {
        let t = raw.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        let lines = t.lines().count();
        if lines >= PASTE_LINES || t.chars().count() >= PASTE_CHARS {
            let tag = format!("[Pasted text #{}: {} lines]", self.pastes.len() + 1, lines.max(1));
            self.pastes.push((tag.clone(), t));
            self.insert(&tag);
        } else {
            self.insert(&t);
        }
    }

    /// Visual lines of the composer as char ranges: split at newlines, hard-wrapped at `width`.
    pub fn layout(&self, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let mut out = vec![];
        let (mut start, mut w, mut i) = (0, 0, 0);
        for c in self.input.chars() {
            if c == '\n' {
                out.push((start, i));
                start = i + 1;
                w = 0;
            } else {
                let cw = c.width().unwrap_or(0);
                if w + cw > width {
                    out.push((start, i));
                    start = i;
                    w = 0;
                }
                w += cw;
            }
            i += 1;
        }
        out.push((start, i));
        out
    }

    /// (visual line, column in display cells) of the cursor.
    pub fn cursor_pos(&self, width: usize) -> (usize, usize) {
        let lines = self.layout(width);
        let chars: Vec<char> = self.input.chars().collect();
        // At a wrap point the cursor belongs to the start of the next line.
        let idx = lines.iter().rposition(|&(a, _)| a <= self.cursor).unwrap_or(0);
        let (a, _) = lines[idx];
        (idx, chars[a..self.cursor].iter().map(|c| c.width().unwrap_or(0)).sum())
    }

    fn move_vertical(&mut self, up: bool) -> bool {
        let lines = self.layout(self.comp_w);
        let (row, here) = self.cursor_pos(self.comp_w);
        let col = *self.goal_col.get_or_insert(here);
        let target = if up { row.checked_sub(1) } else { (row + 1 < lines.len()).then_some(row + 1) };
        let Some(t) = target else { return false };
        let (a, b) = lines[t];
        let chars: Vec<char> = self.input.chars().collect();
        let (mut x, mut w) = (a, 0);
        while x < b && w + chars[x].width().unwrap_or(0) <= col {
            w += chars[x].width().unwrap_or(0);
            x += 1;
        }
        self.cursor = x;
        true
    }

    fn history_step(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.hist, older) {
            (None, true) => {
                self.draft = self.input.clone();
                Some(self.history.len() - 1)
            }
            (None, false) => return,
            (Some(0), true) => Some(0),
            (Some(i), true) => Some(i - 1),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.hist = next;
        self.input = match next {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.len_chars();
    }

    fn token_before_cursor(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let token: String = chars[start..self.cursor].iter().collect();
        match token.chars().next()? {
            '@' => Some((start, token)),
            '/' if chars[..start].iter().all(|c| c.is_whitespace()) => Some((start, token)),
            _ => None,
        }
    }

    /// Recompute the completion popup for the token under the cursor.
    pub fn refresh_popup(&mut self) {
        let found = self.token_before_cursor();
        let Some((start, token)) = found else {
            self.popup = None;
            self.dismissed = None;
            return;
        };
        if self.dismissed.as_deref() == Some(token.as_str()) {
            self.popup = None;
            return;
        }
        self.dismissed = None;
        let items = self.completer.as_ref().map(|c| c(&token)).unwrap_or_default();
        if items.is_empty() {
            self.popup = None;
            return;
        }
        let sel = match &self.popup {
            Some(p) if p.token == token => p.sel.min(items.len() - 1),
            _ => 0,
        };
        self.popup = Some(Popup { items, sel, start, token });
    }

    fn accept_completion(&mut self) {
        let Some(p) = self.popup.take() else { return };
        let item = &p.items[p.sel];
        let keep_open = item.insert.ends_with('/');
        let mut text = item.insert.clone();
        if !keep_open {
            text.push(' ');
        }
        let (a, b) = (self.byte_at(p.start), self.byte_at(self.cursor));
        self.input.replace_range(a..b, &text);
        self.cursor = p.start + text.chars().count();
        self.refresh_popup();
    }

    fn submit(&mut self) -> Action {
        let shown = self.input.trim().to_string();
        let mut text = shown.clone();
        for (tag, content) in &self.pastes {
            text = text.replace(tag, content.trim_end());
        }
        self.input.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.popup = None;
        self.hist = None;
        self.draft.clear();
        if self.history.last() != Some(&shown) {
            self.history.push(shown.clone());
        }
        Action::Submit { text, shown }
    }

    fn key(&mut self, k: KeyEvent) -> Action {
        let action = self.key_inner(k);
        if self.dialogs.is_empty() {
            self.refresh_popup();
        }
        action
    }

    fn key_inner(&mut self, k: KeyEvent) -> Action {
        if k.kind == KeyEventKind::Release {
            return Action::None;
        }
        self.hint = None;
        if !self.dialogs.is_empty() {
            return self.dialog_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if !matches!(k.code, KeyCode::Up | KeyCode::Down) {
            self.goal_col = None;
        }
        let page = self.view_h.saturating_sub(1).max(1);
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.running {
                    return Action::Interrupt;
                }
                if self.input.is_empty() {
                    return Action::Quit;
                }
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() && !self.running {
                    return Action::Quit;
                }
                if self.cursor < self.len_chars() {
                    self.delete_range(self.cursor, self.cursor + 1);
                }
            }
            KeyCode::Esc => {
                if let Some(p) = self.popup.take() {
                    self.dismissed = Some(p.token);
                    return Action::None;
                }
                if self.running {
                    return Action::Interrupt;
                }
                self.scroll = 0;
            }
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if alt => self.insert("\n"),
            KeyCode::Tab => self.accept_completion(),
            KeyCode::Enter => {
                if let Some(p) = &self.popup {
                    // Enter accepts the highlighted candidate, unless it is already what was typed.
                    if p.items[p.sel].insert != p.token {
                        self.accept_completion();
                        return Action::None;
                    }
                }
                if self.running {
                    self.hint = Some("a run is in progress: Esc interrupts it".into());
                    return Action::None;
                }
                if self.input.trim().is_empty() {
                    return Action::Poke;
                }
                return self.submit();
            }
            KeyCode::Up | KeyCode::Down => {
                let up = k.code == KeyCode::Up;
                if let Some(p) = self.popup.as_mut() {
                    let n = p.items.len();
                    p.sel = if up { (p.sel + n - 1) % n } else { (p.sel + 1) % n };
                } else if !self.move_vertical(up) {
                    self.history_step(up);
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(page),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::End if ctrl => self.scroll = 0,
            KeyCode::Home if ctrl => self.scroll = usize::MAX / 2,
            KeyCode::Left if alt || ctrl => self.cursor = self.word_start(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_end(),
            KeyCode::Char('b') if alt => self.cursor = self.word_start(),
            KeyCode::Char('f') if alt => self.cursor = self.word_end(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len_chars()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len_chars(),
            KeyCode::Char('u') if ctrl => self.delete_range(self.line_start(), self.cursor),
            KeyCode::Char('k') if ctrl => {
                // To the end of the line; at the end of a line, the line break itself.
                let end = self.line_end();
                self.delete_range(self.cursor, if end == self.cursor { (end + 1).min(self.len_chars()) } else { end });
            }
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Backspace if alt => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.delete_range(self.cursor - 1, self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.len_chars() {
                    self.delete_range(self.cursor, self.cursor + 1);
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
            _ => {}
        }
        Action::None
    }

    fn dialog_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c')) {
            // Rejecting stops the run; the interrupt flag stops anything still waiting.
            self.answer(Reply::Reject(None));
            return Action::Interrupt;
        }
        let Some(d) = self.dialogs.front_mut() else { return Action::None };
        if let Some(f) = d.feedback.as_mut() {
            match k.code {
                KeyCode::Enter => {
                    let text = std::mem::take(f);
                    self.answer(Reply::Reject((!text.trim().is_empty()).then(|| text.trim().to_string())));
                }
                KeyCode::Esc => d.feedback = None,
                KeyCode::Backspace => {
                    f.pop();
                }
                KeyCode::Char(c) if !ctrl => f.push(c),
                _ => {}
            }
            return Action::None;
        }
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.answer(Reply::Once),
            KeyCode::Char('a') | KeyCode::Char('A') => self.answer(Reply::Always),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer(Reply::Reject(None)),
            KeyCode::Char('f') | KeyCode::Char('F') => d.feedback = Some(String::new()),
            _ => {}
        }
        Action::None
    }

    fn answer(&mut self, reply: Reply) {
        if let Some(d) = self.dialogs.pop_front() {
            let what = match &reply {
                Reply::Once => "allowed once",
                Reply::Always => "always allowed",
                Reply::Reject(Some(_)) => "rejected with feedback",
                Reply::Reject(None) => "rejected",
            };
            self.push_info(format!("{} {}: {}", d.action, d.resources.join(", "), what));
            let _ = d.reply.send(reply);
        }
    }

    // ---- rendering ----

    /// Transcript rows as wrapped, styled lines for a given width.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let width = width.max(8);
        let mut out: Vec<Line<'static>> = vec![];
        for (i, row) in self.rows.iter().enumerate() {
            match row {
                Row::User(s) => {
                    if i > 0 {
                        out.push(Line::default());
                    }
                    for (n, l) in wrap(s, width - 2).into_iter().enumerate() {
                        out.push(Line::from(vec![Span::styled(if n == 0 { "> " } else { "  " }, t.user), Span::styled(l, t.text)]));
                    }
                }
                Row::Assistant(s) => {
                    for l in wrap(s.trim_end(), width) {
                        out.push(Line::styled(l, t.text));
                    }
                }
                Row::Tool { depth, agent, name, summary, state } => {
                    let pad = "  ".repeat(*depth);
                    let who = if *depth > 0 { format!("[{agent}] ") } else { String::new() };
                    for l in wrap(&format!("{pad}{who}> {name} {summary}"), width) {
                        out.push(Line::styled(l, t.dim));
                    }
                    if let ToolState::Failed(msg) = state {
                        for l in wrap(&format!("{pad}  failed: {}", msg.lines().next().unwrap_or("")), width) {
                            out.push(Line::styled(l, t.error));
                        }
                    }
                }
                Row::Todos(todos) => {
                    for td in todos {
                        let mark = match td.status.as_str() {
                            "completed" => "[x]",
                            "in_progress" => "[~]",
                            "cancelled" => "[-]",
                            _ => "[ ]",
                        };
                        for l in wrap(&format!("  {mark} {}", td.content), width) {
                            out.push(Line::styled(l, t.dim));
                        }
                    }
                }
                Row::Shell(s) => out.extend(wrap(s.trim_end(), width).into_iter().map(|l| Line::styled(l, t.text))),
                Row::Info(s) => out.extend(wrap(s, width).into_iter().map(|l| Line::styled(l, t.dim))),
                Row::Error(s) => out.extend(wrap(s, width).into_iter().map(|l| Line::styled(l, t.error))),
                Row::Usage(s) => out.extend(wrap(s, width).into_iter().map(|l| Line::styled(l, t.dim))),
            }
        }
        out
    }

    fn status_text(&self, width: usize) -> String {
        let spin = ["|", "/", "-", "\\"][(self.tick % 4) as usize];
        let mut parts = vec![self.model.clone(), format!("agent {}", self.agent)];
        if let Some(t) = &self.title {
            parts.push(t.clone());
        }
        if self.session_cost > 0.0 {
            parts.push(format!("${:.4}", self.session_cost));
        }
        if self.context > 0 {
            parts.push(match self.limit {
                Some(l) if l > 0 => format!("ctx {}%", (self.context * 100 / l).min(100)),
                _ => format!("ctx {}", human(self.context)),
            });
        }
        let mut s = format!(" {}", parts.join(" | "));
        let right = if let Some(h) = &self.hint {
            h.clone()
        } else if self.running {
            format!("{spin} running (Esc interrupts)")
        } else if self.scroll > 0 {
            "scrolled (Ctrl-End follows)".to_string()
        } else {
            "Enter sends, Ctrl-D quits".to_string()
        };
        let used = display_width(&s) + display_width(&right) + 2;
        if used < width {
            s.push_str(&" ".repeat(width - used + 1));
            s.push_str(&right);
        } else {
            s = clip_width(&s, width.saturating_sub(display_width(&right) + 2));
            s.push(' ');
            s.push_str(&right);
        }
        s
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn clip_width(s: &str, max: usize) -> String {
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

/// Word wrap by display width. Control characters other than newline and tab are dropped, so
/// model output cannot move the cursor or recolor the screen.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for raw in text.split('\n') {
        let raw: String = raw.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect();
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut w = 0;
        let mut last_space: Option<usize> = None; // byte index of the last space in `line`
        for c in raw.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                if c == ' ' {
                    // The line is exactly full: break here instead of at an earlier space.
                    out.push(std::mem::take(&mut line));
                    w = 0;
                    last_space = None;
                    continue;
                }
                match last_space {
                    Some(i) if i > 0 => {
                        out.push(line[..i].trim_end().to_string());
                        line = line[i + 1..].to_string();
                        w = display_width(&line);
                    }
                    _ => {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                }
                last_space = None;
            }
            if c == ' ' {
                last_space = Some(line.len());
            }
            line.push(c);
            w += cw;
        }
        out.push(line);
    }
    out
}

fn dialog_lines(d: &Permission, width: usize) -> Vec<Line<'static>> {
    let mut v = vec![Line::styled(format!("Permission needed: {}", d.action), Style::new().add_modifier(Modifier::BOLD))];
    for r in d.resources.iter().take(4) {
        v.push(Line::raw(format!("  {}", clip_width(r.lines().next().unwrap_or(""), width.saturating_sub(4)))));
    }
    if d.resources.len() > 4 {
        v.push(Line::raw(format!("  ... {} more", d.resources.len() - 4)));
    }
    match &d.feedback {
        Some(f) => v.push(Line::raw(format!("feedback (Enter sends, Esc back): {f}"))),
        None => {
            let always = if d.save.is_empty() { String::new() } else { format!(" ({})", clip_width(&d.save.join(", "), width.saturating_sub(40))) };
            v.push(Line::styled(format!("[y] once  [a] always{always}  [n] reject  [f] reject with feedback"), Style::new().fg(Color::Cyan)));
        }
    }
    v
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let dialog = app.dialogs.front().map(|d| dialog_lines(d, area.width as usize));
    let comp_w = (area.width as usize).saturating_sub(2).max(1);
    app.comp_w = comp_w;
    let comp_lines = app.layout(comp_w);
    let visible = comp_lines.len().min(MAX_COMPOSER_LINES);
    let bottom_h = dialog.as_ref().map_or(visible as u16 + 2, |l| l.len() as u16 + 2).min(area.height.saturating_sub(2).max(1));
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_h), Constraint::Length(1)]).split(area);
    let (main, bottom, status) = (parts[0], parts[1], parts[2]);

    let lines = app.lines(main.width as usize);
    let h = main.height as usize;
    app.view_h = h;
    app.scroll = app.scroll.min(lines.len().saturating_sub(h));
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(h);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), main);

    if let Some(l) = dialog {
        f.render_widget(Paragraph::new(l).block(Block::bordered().border_style(app.theme.accent)), bottom);
    } else {
        let title = if app.running { " moco (running) " } else { " moco " };
        let block = Block::bordered().title(title).border_style(app.theme.accent);
        let inner = block.inner(bottom);
        let chars: Vec<char> = app.input.chars().collect();
        let (row, col) = app.cursor_pos(comp_w);
        // Show the window of lines that contains the cursor.
        let shown = (inner.height as usize).max(1);
        let top = (row + 1).saturating_sub(shown);
        let rows: Vec<Line> = comp_lines.iter().skip(top).take(shown).map(|&(a, b)| Line::raw(chars[a..b].iter().collect::<String>())).collect();
        f.render_widget(Paragraph::new(rows).block(block), bottom);
        f.set_cursor_position((inner.x + (col as u16).min(inner.width.saturating_sub(1)), inner.y + (row - top) as u16));
    }
    if let Some(p) = &app.popup {
        let n = p.items.len().min(MAX_POPUP) as u16;
        let width = main.width.min(70);
        let h = n.min(main.height);
        let rect = Rect::new(main.x, main.y + main.height - h, width, h);
        let first = (p.sel + 1).saturating_sub(MAX_POPUP);
        let lines: Vec<Line> = p
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(MAX_POPUP)
            .map(|(i, it)| {
                let text = clip_width(&format!(" {}  {}", it.label, it.detail), width as usize);
                let pad = " ".repeat((width as usize).saturating_sub(display_width(&text)));
                Line::styled(format!("{text}{pad}"), if i == p.sel { app.theme.bar } else { app.theme.dim.add_modifier(Modifier::REVERSED) })
            })
            .collect();
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(lines), rect);
    }
    f.render_widget(Paragraph::new(app.status_text(status.width as usize)).style(app.theme.bar), status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use moco_core::engine::RunOutcome;
    use ratatui::backend::TestBackend;
    use std::sync::mpsc::channel;

    fn key(code: KeyCode, mods: KeyModifiers) -> AppEvent {
        AppEvent::Key(KeyEvent { code, modifiers: mods, kind: KeyEventKind::Press, state: KeyEventState::NONE })
    }
    fn press(app: &mut App, c: KeyCode) -> Action {
        app.update(key(c, KeyModifiers::NONE))
    }
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }
    fn text(app: &mut App, ev: Ev) {
        app.update(AppEvent::Engine { depth: 0, agent: String::new(), ev });
    }
    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
        let b = t.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn wrap_breaks_at_spaces_and_hard_wraps_long_words() {
        assert_eq!(wrap("hello brave new world", 11), vec!["hello brave", "new world"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("日本語です", 4), vec!["日本", "語で", "す"]);
        assert_eq!(wrap("x\x1b[31mred\x07", 20), vec!["x[31mred"]); // escapes cannot reach the terminal
    }

    #[test]
    fn composer_edits_at_the_cursor() {
        let mut a = App::new("m", "build");
        type_str(&mut a, "helo wrld");
        a.update(key(KeyCode::Left, KeyModifiers::NONE));
        a.update(key(KeyCode::Left, KeyModifiers::NONE));
        a.update(key(KeyCode::Left, KeyModifiers::NONE));
        a.update(key(KeyCode::Left, KeyModifiers::NONE));
        type_str(&mut a, "o");
        assert_eq!(a.input, "helo owrld");
        let mut a = App::new("m", "build");
        type_str(&mut a, "one two three");
        a.update(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(a.input, "one two ");
        a.update(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(a.input, "");
        type_str(&mut a, "ab");
        a.update(key(KeyCode::Enter, KeyModifiers::ALT));
        type_str(&mut a, "cd");
        a.update(key(KeyCode::Char('a'), KeyModifiers::CONTROL)); // start of this line, not of the text
        type_str(&mut a, ">");
        a.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(a.input, "ab\n>");
        a.update(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        a.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL)); // at a line end: joins nothing after, removes nothing
        a.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(a.input, "ab");
        a.update(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        a.update(AppEvent::Paste("multi\nline\r\npaste".into()));
        assert_eq!(a.input, "multi\nline\npaste"); // short pastes keep their lines
        type_str(&mut a, "é");
        a.update(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(a.input, "multi\nline\npaste");
    }

    #[test]
    fn enter_submits_only_when_idle_and_ctrl_c_depends_on_state() {
        let mut a = App::new("m", "build");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Poke);
        type_str(&mut a, "  fix it  ");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Submit { text: "fix it".into(), shown: "fix it".into() });
        assert_eq!(a.input, "");
        a.running = true;
        type_str(&mut a, "more");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::None);
        assert!(a.hint.is_some());
        assert_eq!(a.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::Interrupt);
        assert_eq!(press(&mut a, KeyCode::Esc), Action::Interrupt);
        a.running = false;
        assert_eq!(a.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::None); // clears "more"
        assert_eq!(a.input, "");
        assert_eq!(a.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::Quit);
    }

    #[test]
    fn streamed_text_appends_to_one_row_and_tools_finish_in_place() {
        let mut a = App::new("m", "build");
        a.push_user("hi");
        text(&mut a, Ev::Text("Hel".into()));
        text(&mut a, Ev::Text("lo".into()));
        text(&mut a, Ev::Tool { name: "read".into(), summary: "a.rs".into() });
        text(&mut a, Ev::Tool { name: "read".into(), summary: "b.rs".into() });
        text(&mut a, Ev::ToolDone { name: "read".into(), ok: false, msg: "no such file\nmore".into() });
        text(&mut a, Ev::Text("done".into()));
        assert_eq!(a.rows.len(), 5);
        let states: Vec<_> = a.rows.iter().filter_map(|r| if let Row::Tool { state, .. } = r { Some(state.clone()) } else { None }).collect();
        assert_eq!(states, vec![ToolState::Running, ToolState::Failed("no such file\nmore".into())]); // the newest running call finishes first
        let s = screen(&mut a, 40, 12);
        let joined = s.join("\n");
        assert!(joined.contains("> hi") && joined.contains("Hello") && joined.contains("> read a.rs") && joined.contains("failed: no such file"), "{joined}");
        // Subagent prose is hidden, its tools are indented.
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Text("secret".into()) });
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Tool { name: "grep".into(), summary: "x".into() } });
        let joined = screen(&mut a, 40, 14).join("\n");
        assert!(!joined.contains("secret") && joined.contains("  [explore] > grep x"), "{joined}");
    }

    #[test]
    fn scrolling_follows_the_tail_until_the_user_scrolls_up() {
        let mut a = App::new("m", "build");
        for i in 0..30 {
            text(&mut a, Ev::Notice(format!("line {i}")));
        }
        let s = screen(&mut a, 30, 10); // 10 - 3 composer - 1 status = 6 transcript lines
        assert_eq!(s[5], "line 29");
        press(&mut a, KeyCode::PageUp);
        let s = screen(&mut a, 30, 10);
        assert_eq!(s[5], "line 24"); // one page (view - 1 = 5 lines) up
        assert!(s[9].contains("scrolled"));
        a.update(key(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(screen(&mut a, 30, 10)[5], "line 29");
        a.update(key(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(screen(&mut a, 30, 10)[0], "line 0"); // clamped to the top
    }

    #[test]
    fn permission_dialog_answers_the_engine_and_queues() {
        let mut a = App::new("m", "build");
        a.running = true;
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec!["rm -rf x".into()], save: vec!["rm *".into()], reply: tx1 });
        a.update(AppEvent::Ask { action: "edit".into(), resources: vec!["a.rs".into()], save: vec![], reply: tx2 });
        let joined = screen(&mut a, 70, 12).join("\n");
        assert!(joined.contains("Permission needed: shell") && joined.contains("rm -rf x") && joined.contains("[a] always (rm *)"), "{joined}");
        type_str(&mut a, "a");
        assert_eq!(rx1.try_recv().unwrap(), Reply::Always);
        assert!(screen(&mut a, 70, 12).join("\n").contains("Permission needed: edit")); // the next one is shown
        press(&mut a, KeyCode::Char('f'));
        type_str(&mut a, "use sed");
        press(&mut a, KeyCode::Backspace);
        type_str(&mut a, "d");
        press(&mut a, KeyCode::Enter);
        assert_eq!(rx2.try_recv().unwrap(), Reply::Reject(Some("use sed".into())));
        assert!(a.dialogs.is_empty());
        // Typing during a dialog never reaches the composer.
        assert_eq!(a.input, "");
    }

    #[test]
    fn run_done_reports_outcome_and_usage() {
        let mut a = App::new("m", "build");
        a.running = true;
        text(&mut a, Ev::Usage { usage: Usage { input: 1200, output: 80, cache_read: 4000, cache_write: 0 }, cost: Some(0.0123), session_cost: 0.05, context: 5000, limit: Some(100_000) });
        a.update(AppEvent::RunDone(Ok(RunOutcome::Interrupted)));
        assert!(!a.running);
        let joined = screen(&mut a, 90, 10).join("\n");
        assert!(joined.contains("interrupted") && joined.contains("1200 in, 80 out, 4000 cached | $0.0123 (session $0.0500) | context 5% of 100.0k"), "{joined}");
        assert!(joined.contains("$0.0500") && joined.contains("ctx 5%"), "{joined}");
        // A pending dialog dies with its run.
        let (tx, _rx) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec![], save: vec![], reply: tx });
        a.update(AppEvent::RunDone(Ok(RunOutcome::Idle)));
        assert!(a.dialogs.is_empty());
    }

    fn fake_completer(a: &mut App) {
        a.completer = Some(Box::new(|t: &str| {
            let all = ["/review", "/init", "@src/", "@src/main.rs", "@README.md"];
            all.iter().filter(|c| c.starts_with(t)).map(|c| Item { insert: c.to_string(), label: c.to_string(), detail: String::new() }).collect()
        }));
    }

    #[test]
    fn alt_enter_adds_a_line_and_up_down_move_between_lines_then_history() {
        let mut a = App::new("m", "build");
        render_once(&mut a);
        type_str(&mut a, "first");
        a.update(key(KeyCode::Enter, KeyModifiers::ALT));
        a.update(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_str(&mut a, "third");
        assert_eq!(a.input, "first\n\nthird");
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Up);
        type_str(&mut a, "X"); // cursor moved to the first line, column 5
        assert_eq!(a.input, "firstX\n\nthird");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Submit { text: "firstX\n\nthird".into(), shown: "firstX\n\nthird".into() });
        // History: Up from an empty composer recalls the last prompt; Down restores the draft.
        type_str(&mut a, "draft");
        press(&mut a, KeyCode::Up);
        assert_eq!(a.input, "firstX\n\nthird");
        type_str(&mut a, ""); // still on the history entry
        press(&mut a, KeyCode::Down); // cursor is at the end (last line): leave history
        assert_eq!(a.input, "draft");
        press(&mut a, KeyCode::Down);
        assert_eq!(a.input, "draft"); // nothing newer
        // Several entries, oldest first when walking up; consecutive duplicates are not stored twice.
        let mut a = App::new("m", "build");
        render_once(&mut a);
        for t in ["one", "two", "two", "three"] {
            type_str(&mut a, t);
            press(&mut a, KeyCode::Enter);
        }
        assert_eq!(a.history, vec!["one", "two", "three"]);
        for want in ["three", "two", "one", "one"] {
            press(&mut a, KeyCode::Up);
            assert_eq!(a.input, want);
        }
    }

    fn render_once(a: &mut App) {
        screen(a, 40, 12);
    }

    #[test]
    fn wrapped_lines_move_the_cursor_visually() {
        let mut a = App::new("m", "build");
        screen(&mut a, 12, 12); // composer inner width 10
        type_str(&mut a, "abcdefghijklmno"); // wraps after 10 chars
        assert_eq!(a.layout(10), vec![(0, 10), (10, 15)]);
        assert_eq!(a.cursor_pos(10), (1, 5));
        press(&mut a, KeyCode::Up);
        assert_eq!(a.cursor, 5);
        press(&mut a, KeyCode::Down);
        assert_eq!(a.cursor, 15);
        let s = screen(&mut a, 12, 12);
        assert!(s.iter().any(|l| l.contains("abcdefghij")) && s.iter().any(|l| l.contains("klmno")), "{s:?}");
    }

    #[test]
    fn big_pastes_collapse_to_a_placeholder_and_expand_on_send() {
        let mut a = App::new("m", "build");
        let big = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        type_str(&mut a, "look: ");
        a.update(AppEvent::Paste(big.clone()));
        assert_eq!(a.input, "look: [Pasted text #1: 20 lines]");
        type_str(&mut a, " thanks");
        match press(&mut a, KeyCode::Enter) {
            Action::Submit { text, shown } => {
                assert_eq!(text, format!("look: {big} thanks"));
                assert_eq!(shown, "look: [Pasted text #1: 20 lines] thanks");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn completion_popup_filters_accepts_and_dismisses() {
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "/");
        assert_eq!(a.popup.as_ref().unwrap().items.len(), 2);
        type_str(&mut a, "re");
        assert_eq!(a.popup.as_ref().unwrap().items.len(), 1);
        let joined = screen(&mut a, 40, 10).join("\n");
        assert!(joined.contains("/review"), "{joined}");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::None); // accepts instead of sending
        assert_eq!(a.input, "/review ");
        assert!(a.popup.is_none());
        type_str(&mut a, "now @");
        assert_eq!(a.popup.as_ref().unwrap().items.len(), 3);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Up); // selection moves: index 1
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.input, "/review now @src/main.rs ");
        // Directories keep the popup open for the next level; Esc dismisses until the token changes.
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "@sr");
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.input, "@src/"); // a directory keeps the token open for the next level
        assert_eq!(a.popup.as_ref().unwrap().items.len(), 2);
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "@zz");
        assert!(a.popup.is_none()); // nothing matches
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "@src");
        assert!(a.popup.is_some());
        press(&mut a, KeyCode::Esc);
        assert!(a.popup.is_none());
        type_str(&mut a, "/");
        assert!(a.popup.is_some()); // a new token completes again
        // Not a command or mention: no popup (a path in the middle of a sentence, an email address).
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "see /re and a@re");
        assert!(a.popup.is_none());
        // An exact command sends instead of re-accepting.
        let mut a = App::new("m", "build");
        fake_completer(&mut a);
        type_str(&mut a, "/init");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Submit { text: "/init".into(), shown: "/init".into() });
    }

    #[test]
    fn shell_output_and_the_composer_height() {
        let mut a = App::new("m", "build");
        a.running = true;
        a.update(AppEvent::ShellDone(Ok("hello\nworld".into())));
        assert!(!a.running);
        let joined = screen(&mut a, 30, 12).join("\n");
        assert!(joined.contains("hello\nworld"), "{joined}");
        // The composer grows with its content up to a limit.
        for _ in 0..20 {
            a.update(key(KeyCode::Enter, KeyModifiers::ALT));
        }
        let s = screen(&mut a, 30, 20);
        assert_eq!(s.iter().filter(|l| l.starts_with('│')).count(), 8, "{s:?}");
    }

    #[test]
    fn the_cursor_stays_visible_in_a_long_line() {
        let mut a = App::new("m", "build");
        type_str(&mut a, &"x".repeat(50));
        type_str(&mut a, "END");
        let s = screen(&mut a, 20, 8);
        assert!(s[s.len() - 3].contains("xxxEND"), "{s:?}");
    }
}
