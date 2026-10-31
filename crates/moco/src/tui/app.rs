//! The TUI model: state, the pure `update` function and `render`. No terminal or engine in here,
//! so tests drive it with scripted events and read ratatui's test buffer.
use crate::human;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use moco_core::engine::Reply;
use moco_core::provider::Usage;
use moco_core::tools::{Question, Todo};
use ratatui::{prelude::*, widgets::*};
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use super::glyphs::Glyphs;
use super::keys::{KeyAction, Keymap};
use super::md;
use unicode_width::UnicodeWidthChar;

pub use super::theme::Theme;

/// An engine event converted to owned data (the engine's `UiEvent` borrows).
#[derive(Debug, Clone)]
pub enum Ev {
    Text(String),
    Tool { name: String, summary: String, detail: Option<String> },
    ToolDone { name: String, ok: bool, msg: String, output: String },
    Reasoning(String),
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
    /// The terminal window gained or lost focus.
    Focus(bool),
    /// The engine delivered waiting prompts (the UI syncs its queue and shows them).
    Delivered,
    /// A line from the engine's notifier (background job finished, ...).
    Background(String),
    RunDone(Result<moco_core::engine::RunOutcome, String>),
    /// A `!command` the user ran finished.
    ShellDone(Result<String, String>),
    /// The `question` tool wants answers; `None` back means dismissed.
    Questions { questions: Vec<Question>, reply: Sender<Option<Vec<Vec<String>>>> },
    /// A background operation (compaction, ...) finished with a message for the transcript.
    OpDone(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    Model,
    Session,
    Agent,
    Command,
    /// Prompts waiting in the inbox: Enter switches steer/queue, Ctrl-D removes.
    Queue,
    /// Background jobs: Enter shows the output, Ctrl-D stops one.
    Job,
    Theme,
}

/// A prompt that was sent while a turn was running and has not been delivered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub id: String,
    /// Delivered at the next step boundary (true) or when the turn is over (false).
    pub steer: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub value: String,
}

/// A filterable list the user picks one entry from.
pub struct Picker {
    pub kind: PickKind,
    pub title: String,
    pub items: Vec<PickItem>,
    pub filter: String,
    pub sel: usize,
}

impl Picker {
    pub fn new(kind: PickKind, title: &str, items: Vec<PickItem>, filter: &str) -> Picker {
        Picker { kind, title: title.to_string(), items, filter: filter.to_string(), sel: 0 }
    }

    /// Indices of the items matching every word of the filter.
    pub fn visible(&self) -> Vec<usize> {
        let words: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        (0..self.items.len())
            .filter(|&i| {
                let hay = format!("{} {}", self.items[i].label, self.items[i].detail).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }
}

/// One changed file in the diff viewer.
pub struct ViewFile {
    pub path: String,
    pub adds: usize,
    pub dels: usize,
    pub binary: bool,
    pub patch: String,
}

/// Full-screen view of the session's changes.
pub struct Viewer {
    pub files: Vec<ViewFile>,
    pub scroll: usize,
}

impl Viewer {
    /// Wrapped, colored lines, and the line each file starts at.
    fn lines(&self, width: usize, t: &Theme, g: &Glyphs) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut out = vec![];
        let mut starts = vec![];
        let card = t.text.patch(t.panel).add_modifier(Modifier::BOLD);
        for f in &self.files {
            starts.push(out.len());
            let spans = vec![
                Span::styled(g.user_bar, t.accent.patch(t.panel)),
                Span::styled(format!(" {}  ", f.path), card),
                Span::styled(format!("+{}", f.adds), t.added.fg.map_or(card, |c| card.fg(c))),
                Span::styled(" ", card),
                Span::styled(format!("-{}", f.dels), t.removed.fg.map_or(card, |c| card.fg(c))),
            ];
            out.push(fill(spans, width, t.panel));
            if f.binary {
                out.push(Line::styled("binary file changed", t.dim));
            }
            for raw in f.patch.lines() {
                out.extend(diff_line(raw, width, t, g));
            }
            out.push(Line::default());
        }
        (out, starts)
    }
}

/// The `question` tool's form: one question at a time.
pub struct Form {
    pub questions: Vec<Question>,
    pub qi: usize,
    pub cur: usize,
    pub checked: Vec<bool>,
    /// Some while typing a free-text answer.
    pub typing: Option<String>,
    pub answers: Vec<Vec<String>>,
    pub reply: Sender<Option<Vec<Vec<String>>>>,
}

impl Form {
    fn new(questions: Vec<Question>, reply: Sender<Option<Vec<Vec<String>>>>) -> Form {
        let n = questions.first().map_or(0, |q| q.options.len());
        Form { questions, qi: 0, cur: 0, checked: vec![false; n], typing: None, answers: vec![], reply }
    }

    fn q(&self) -> &Question {
        &self.questions[self.qi]
    }
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
/// Lines of an edit's diff shown inline at the normal detail level.
const DETAIL_LINES: usize = 24;
const SIDEBAR_W: u16 = 32;

/// What the event loop must do after an update.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// `text` has pasted blocks expanded; `shown` is what the transcript displays.
    Submit { text: String, shown: String },
    /// Enter on an empty composer: let the assistant react to queued background results.
    Poke,
    /// Ctrl-P: show the command palette.
    Palette,
    /// An entry was chosen in a picker.
    Picked { kind: PickKind, value: String },
    /// Ctrl-D on an entry of a queue or job picker.
    Dropped { kind: PickKind, value: String },
    /// A prompt typed while a turn runs: steer it into the turn, or queue it for after.
    Enqueue { text: String, shown: String, queue: bool },
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
    Tool { depth: usize, agent: String, name: String, summary: String, state: ToolState, detail: Option<String>, output: String },
    /// The model's reasoning, shown according to the detail level.
    Reasoning(String),
    /// The welcome card at the top of a session.
    Banner { title: String, model: String, agent: String, cwd: String },
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
    pub forms: VecDeque<Form>,
    pub picker: Option<Picker>,
    pub viewer: Option<Viewer>,
    pub model: String,
    pub agent: String,
    pub title: Option<String>,
    pub session_cost: f64,
    pub context: u64,
    pub limit: Option<u64>,
    pub hint: Option<String>,
    pub tick: u64,
    pub queued: Vec<Queued>,
    pub jobs: usize,
    /// A retry is pending: when the next attempt starts, which attempt, and why.
    pub retry: Option<(std::time::Instant, u32, String)>,
    /// Detail level: 0 compact, 1 normal, 2 full (Ctrl-O cycles).
    pub level: u8,
    pub keymap: Keymap,
    pub glyphs: &'static Glyphs,
    leader_pending: bool,
    /// `None` shows the sidebar when the terminal is wide enough.
    pub sidebar: Option<bool>,
    side_visible: bool,
    side_fits: Option<bool>,
    /// Latest todo list, MCP server states and background jobs, for the sidebar.
    pub todos: Vec<Todo>,
    pub mcp: Vec<(String, String)>,
    pub job_list: Vec<(String, String)>,
    /// Something happened that deserves attention if the user is away (set by `update`, taken by the loop).
    pub attention: Option<String>,
    pub last_input: std::time::Instant,
    /// `Some` once the terminal has reported focus changes.
    pub focused: Option<bool>,
    pub away_after: std::time::Duration,
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
            forms: VecDeque::new(),
            picker: None,
            viewer: None,
            model: model.to_string(),
            agent: agent.to_string(),
            title: None,
            session_cost: 0.0,
            context: 0,
            limit: None,
            hint: None,
            tick: 0,
            queued: vec![],
            jobs: 0,
            retry: None,
            level: 1,
            keymap: Keymap::default(),
            glyphs: &super::glyphs::UNICODE,
            leader_pending: false,
            sidebar: None,
            side_visible: false,
            side_fits: None,
            todos: vec![],
            mcp: vec![],
            job_list: vec![],
            attention: None,
            last_input: std::time::Instant::now(),
            focused: None,
            away_after: std::time::Duration::from_secs(8),
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
            AppEvent::Key(k) => {
                self.last_input = std::time::Instant::now();
                return self.key(k);
            }
            AppEvent::Focus(f) => {
                self.focused = Some(f);
                if f {
                    self.last_input = std::time::Instant::now();
                }
            }
            AppEvent::Paste(s) => {
                if let Some(Permission { feedback: Some(f), .. }) = self.dialogs.front_mut() {
                    f.push_str(&s.replace(['\r', '\n'], " "));
                } else {
                    self.paste(&s);
                    self.refresh_popup();
                }
            }
            AppEvent::Resize | AppEvent::Delivered => {}
            AppEvent::Tick => self.tick += 1,
            AppEvent::Engine { depth, agent, ev } => self.engine(depth, agent, ev),
            AppEvent::Ask { action, resources, save, reply } => {
                self.attention = Some(format!("{action} needs permission"));
                self.dialogs.push_back(Permission { action, resources, save, reply, feedback: None });
                self.scroll = 0;
            }
            AppEvent::Background(line) => self.push_info(format!("[{line}]")),
            AppEvent::Questions { questions, reply } => {
                if questions.is_empty() {
                    let _ = reply.send(Some(vec![]));
                } else {
                    self.attention = Some("a question needs your answer".into());
                    self.forms.push_back(Form::new(questions, reply));
                    self.scroll = 0;
                }
            }
            AppEvent::OpDone(r) => {
                self.running = false;
                match r {
                    Ok(m) => self.push_info(m),
                    Err(e) => self.rows.push(Row::Error(e)),
                }
            }
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
                self.retry = None;
                self.dialogs.clear(); // a dialog whose run is gone cannot be answered
                self.forms.clear();
                match outcome {
                    Ok(Idle) => self.attention = Some("done".into()),
                    Ok(Failed(_)) => self.attention = Some("the turn failed".into()),
                    Ok(Interrupted) => self.push_info("interrupted"),
                    Ok(Declined) => {
                        self.attention = Some("stopped: a tool call was rejected".into());
                        self.push_info("stopped: tool call rejected")
                    }
                    Ok(Busy) => self.rows.push(Row::Error("session is busy".into())),
                    Err(e) => self.rows.push(Row::Error(format!("store error: {e}"))),
                }
                self.finish_usage();
            }
        }
        Action::None
    }

    /// Is the user probably not looking? The terminal's own focus reports decide when it sends them;
    /// otherwise a stretch without key presses does.
    pub fn away(&self) -> bool {
        match self.focused {
            Some(f) => !f,
            None => self.last_input.elapsed() >= self.away_after,
        }
    }

    fn finish_usage(&mut self) {
        let r = std::mem::take(&mut self.run);
        if r.steps == 0 {
            return;
        }
        let sep = self.glyphs.sep;
        let cost = if r.priced { format!("${:.4} {sep} session ${:.4}", r.cost, self.session_cost) } else { "cost n/a".to_string() };
        let wrote = if r.usage.cache_write > 0 { format!(" {sep} {} cache written", human(r.usage.cache_write)) } else { String::new() };
        self.rows.push(Row::Usage(format!("{} in {sep} {} out {sep} {} cached{wrote} {sep} {cost}", human(r.usage.input), human(r.usage.output), human(r.usage.cache_read))));
    }

    fn engine(&mut self, depth: usize, agent: String, ev: Ev) {
        if !matches!(ev, Ev::Retry { .. }) {
            self.retry = None;
        }
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
            Ev::Tool { name, summary, detail } => self.rows.push(Row::Tool { depth, agent, name, summary, state: ToolState::Running, detail, output: String::new() }),
            Ev::ToolDone { name, ok, msg, output: out } => {
                let found = self.rows.iter_mut().rev().find(|r| matches!(r, Row::Tool { depth: d, name: n, state: ToolState::Running, .. } if *d == depth && *n == name));
                if let Some(Row::Tool { state, output, .. }) = found {
                    *state = if ok { ToolState::Done } else { ToolState::Failed(msg) };
                    *output = out;
                }
            }
            Ev::Reasoning(t) => {
                if depth == 0 {
                    match self.rows.last_mut() {
                        Some(Row::Reasoning(s)) => s.push_str(&t),
                        _ => self.rows.push(Row::Reasoning(t)),
                    }
                }
            }
            Ev::Retry { attempt, secs, error } => {
                self.retry = Some((std::time::Instant::now() + std::time::Duration::from_secs(secs), attempt, error.clone()));
                self.push_info(format!("retrying in {secs}s (attempt {attempt}): {error}"));
            }
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
                    self.todos = t.clone();
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

    /// Take the composer's content: (text with pasted blocks expanded, text as typed).
    fn take_input(&mut self) -> (String, String) {
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
        (text, shown)
    }

    fn submit(&mut self) -> Action {
        let (text, shown) = self.take_input();
        Action::Submit { text, shown }
    }

    fn enqueue(&mut self, queue: bool) -> Action {
        let (text, shown) = self.take_input();
        Action::Enqueue { text, shown, queue }
    }

    /// Commands (`/x`, `!x`) are handled at once even while a turn runs; plain text is queued for the turn.
    fn is_command(&self) -> bool {
        matches!(self.input.trim_start().chars().next(), Some('/') | Some('!'))
    }

    fn key(&mut self, k: KeyEvent) -> Action {
        let action = self.key_inner(k);
        if !self.modal() {
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
        if !self.forms.is_empty() {
            return self.form_key(k);
        }
        if self.viewer.is_some() {
            return self.viewer_key(k);
        }
        if self.picker.is_some() {
            return self.picker_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if !matches!(k.code, KeyCode::Up | KeyCode::Down) {
            self.goal_col = None;
        }
        let page = self.view_h.saturating_sub(1).max(1);
        if self.leader_pending {
            self.leader_pending = false;
            return self.leader_key(&k);
        }
        if !(ctrl && k.code == KeyCode::Char('c')) {
            if let Some(act) = self.keymap.action(&k) {
                if let Some(done) = self.run_action(act, page) {
                    return done;
                }
            }
        }
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
                // (When Ctrl-D is the quit key and nothing is in the way, the keymap already handled it.)
                if self.cursor < self.len_chars() {
                    self.delete_range(self.cursor, self.cursor + 1);
                }
            }
            KeyCode::Tab => self.accept_completion(),
            KeyCode::Enter => {
                if let Some(p) = &self.popup {
                    // Enter accepts the highlighted candidate, unless it is already what was typed.
                    if p.items[p.sel].insert != p.token {
                        self.accept_completion();
                        return Action::None;
                    }
                }
                if self.input.trim().is_empty() {
                    return if self.running { Action::None } else { Action::Poke };
                }
                if self.running && !self.is_command() {
                    return self.enqueue(false);
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

    /// A bound action. `None` lets the key fall through to normal editing.
    fn run_action(&mut self, act: KeyAction, page: usize) -> Option<Action> {
        match act {
            KeyAction::Interrupt => {
                if let Some(p) = self.popup.take() {
                    self.dismissed = Some(p.token);
                    return Some(Action::None);
                }
                if self.running {
                    return Some(Action::Interrupt);
                }
                self.scroll = 0;
                Some(Action::None)
            }
            KeyAction::Quit => (self.input.is_empty() && !self.running).then_some(Action::Quit),
            KeyAction::Palette => Some(Action::Palette),
            KeyAction::Newline => {
                self.insert("\n");
                Some(Action::None)
            }
            KeyAction::Queue => {
                if self.input.trim().is_empty() {
                    return Some(Action::None);
                }
                Some(if self.running && !self.is_command() { self.enqueue(true) } else { self.submit() })
            }
            KeyAction::Detail => {
                self.cycle_detail();
                Some(Action::None)
            }
            KeyAction::Sidebar => {
                self.toggle_sidebar();
                Some(Action::None)
            }
            KeyAction::PageUp => {
                self.scroll = self.scroll.saturating_add(page);
                Some(Action::None)
            }
            KeyAction::PageDown => {
                self.scroll = self.scroll.saturating_sub(page);
                Some(Action::None)
            }
            KeyAction::Top => {
                self.scroll = usize::MAX / 2;
                Some(Action::None)
            }
            KeyAction::Bottom => {
                self.scroll = 0;
                Some(Action::None)
            }
            KeyAction::Leader => {
                self.leader_pending = true;
                self.hint = Some(format!("leader: {}", self.keymap.leader_hint()));
                Some(Action::None)
            }
        }
    }

    fn leader_key(&mut self, k: &KeyEvent) -> Action {
        if k.code == KeyCode::Esc {
            return Action::None;
        }
        match self.keymap.leader_target(k).map(String::from) {
            Some(t) if t.starts_with('/') => Action::Submit { text: t.clone(), shown: t },
            Some(t) => {
                match t.as_str() {
                    "sidebar" => self.toggle_sidebar(),
                    "detail" => self.cycle_detail(),
                    "palette" => return Action::Palette,
                    "quit" => return Action::Quit,
                    _ => {}
                }
                Action::None
            }
            None => {
                self.hint = Some("that key does nothing after the leader (Esc cancels)".into());
                Action::None
            }
        }
    }

    pub fn cycle_detail(&mut self) {
        self.level = (self.level + 1) % 3;
        self.hint = Some(["detail: compact (no reasoning, no diffs)", "detail: normal (diffs, reasoning summarized)", "detail: full (reasoning, tool output, every subagent call)"][self.level as usize].into());
    }

    pub fn sidebar_shown(&self) -> bool {
        self.side_visible
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar = Some(!self.side_visible);
        if !self.side_visible && self.side_fits == Some(false) {
            self.hint = Some("the terminal is too narrow for the sidebar".into());
        }
    }

    /// Is a dialog, form or picker taking the keys?
    pub fn modal(&self) -> bool {
        !self.dialogs.is_empty() || !self.forms.is_empty() || self.picker.is_some() || self.viewer.is_some()
    }

    pub fn open_picker(&mut self, p: Picker) {
        self.popup = None;
        self.picker = Some(p);
    }

    fn viewer_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.view_h.saturating_sub(1).max(1);
        let width = self.comp_w + 2;
        let Some(v) = self.viewer.as_mut() else { return Action::None };
        let (lines, starts) = v.lines(width, &self.theme, self.glyphs);
        let last = lines.len().saturating_sub(self.view_h);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.viewer = None,
            KeyCode::Down | KeyCode::Char('j') => v.scroll = (v.scroll + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => v.scroll = (v.scroll + page).min(last),
            KeyCode::Char('f') if ctrl => v.scroll = (v.scroll + page).min(last),
            KeyCode::PageUp | KeyCode::Char('b') => v.scroll = v.scroll.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => v.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => v.scroll = last,
            KeyCode::Char('n') => {
                if let Some(&s) = starts.iter().find(|&&s| s > v.scroll) {
                    v.scroll = s.min(last);
                }
            }
            KeyCode::Char('p') => {
                if let Some(&s) = starts.iter().rev().find(|&&s| s < v.scroll) {
                    v.scroll = s;
                } else {
                    v.scroll = 0;
                }
            }
            _ => {}
        }
        Action::None
    }

    fn picker_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(p) = self.picker.as_mut() else { return Action::None };
        let n = p.visible().len();
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Char('c') | KeyCode::Char('p') if ctrl => self.picker = None,
            KeyCode::Up => p.sel = if n == 0 { 0 } else { (p.sel + n - 1) % n },
            KeyCode::Down => p.sel = if n == 0 { 0 } else { (p.sel + 1) % n },
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(8),
            KeyCode::PageDown => p.sel = (p.sel + 8).min(n.saturating_sub(1)),
            KeyCode::Backspace => {
                p.filter.pop();
                p.sel = 0;
            }
            KeyCode::Char('u') if ctrl => {
                p.filter.clear();
                p.sel = 0;
            }
            KeyCode::Char('d') if ctrl && matches!(p.kind, PickKind::Queue | PickKind::Job) => {
                let vis = p.visible();
                if let Some(&i) = vis.get(p.sel) {
                    let value = p.items[i].value.clone();
                    let kind = p.kind;
                    p.items.remove(i);
                    p.sel = p.sel.min(p.visible().len().saturating_sub(1));
                    return Action::Dropped { kind, value };
                }
            }
            KeyCode::Char(c) if !ctrl => {
                p.filter.push(c);
                p.sel = 0;
            }
            KeyCode::Enter => {
                let vis = p.visible();
                if let Some(&i) = vis.get(p.sel) {
                    let (kind, value) = (p.kind, p.items[i].value.clone());
                    self.picker = None;
                    return Action::Picked { kind, value };
                }
            }
            _ => {}
        }
        Action::None
    }

    fn form_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c')) {
            self.forms.pop_front(); // dropping the sender dismisses the form
            return Action::Interrupt;
        }
        let Some(f) = self.forms.front_mut() else { return Action::None };
        let n_opts = f.q().options.len();
        let multiple = f.q().multiple;
        if f.typing.is_some() {
            match k.code {
                KeyCode::Esc => f.typing = None,
                KeyCode::Backspace => {
                    f.typing.as_mut().map(|t| t.pop());
                }
                KeyCode::Char(c) if !ctrl => f.typing.as_mut().map(|t| t.push(c)).unwrap_or(()),
                KeyCode::Enter => {
                    let text = f.typing.clone().unwrap_or_default();
                    if !text.trim().is_empty() {
                        let mut picked: Vec<String> = if multiple { (0..n_opts).filter(|&i| f.checked[i]).map(|i| f.q().options[i].label.clone()).collect() } else { vec![] };
                        picked.push(text.trim().to_string());
                        self.confirm(picked);
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        match k.code {
            KeyCode::Esc => {
                self.forms.pop_front();
            }
            KeyCode::Up => f.cur = (f.cur + n_opts) % (n_opts + 1),
            KeyCode::Down => f.cur = (f.cur + 1) % (n_opts + 1),
            KeyCode::Char(' ') if multiple && f.cur < n_opts => f.checked[f.cur] = !f.checked[f.cur],
            KeyCode::Char(d @ '1'..='9') if (d as usize - '1' as usize) < n_opts => {
                let i = d as usize - '1' as usize;
                if multiple {
                    f.checked[i] = !f.checked[i];
                    f.cur = i;
                } else {
                    let label = f.q().options[i].label.clone();
                    self.confirm(vec![label]);
                }
            }
            KeyCode::Enter => {
                if f.cur == n_opts {
                    f.typing = Some(String::new());
                } else if multiple {
                    let mut picked: Vec<String> = (0..n_opts).filter(|&i| f.checked[i]).map(|i| f.q().options[i].label.clone()).collect();
                    if picked.is_empty() {
                        picked.push(f.q().options[f.cur].label.clone());
                    }
                    self.confirm(picked);
                } else {
                    let label = f.q().options[f.cur].label.clone();
                    self.confirm(vec![label]);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Record the answer to the current question; send everything after the last one.
    fn confirm(&mut self, picked: Vec<String>) {
        let Some(f) = self.forms.front_mut() else { return };
        f.answers.push(picked);
        f.qi += 1;
        if f.qi >= f.questions.len() {
            if let Some(f) = self.forms.pop_front() {
                let _ = f.reply.send(Some(f.answers));
            }
        } else {
            f.cur = 0;
            f.typing = None;
            f.checked = vec![false; f.questions[f.qi].options.len()];
        }
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

    /// Groups of rows that render together: a subagent's consecutive tool calls collapse into one line
    /// (unless the detail level is full); every other row stands alone. Cheap: no wrapping happens here.
    fn blocks(&self) -> Vec<(usize, usize)> {
        let mut out = vec![];
        let mut i = 0;
        while i < self.rows.len() {
            let mut j = i + 1;
            if let (Row::Tool { depth, agent, .. }, true) = (&self.rows[i], self.level < 2) {
                if *depth > 0 {
                    while matches!(self.rows.get(j), Some(Row::Tool { depth: d, agent: a, .. }) if d == depth && a == agent) {
                        j += 1;
                    }
                }
            }
            out.push((i, j));
            i = j;
        }
        out
    }

    /// The newest lines of the transcript, wrapped for `width`: only as many rows as needed to produce
    /// `want` lines are rendered, so a long conversation costs no more than a screenful. The flag says
    /// whether the whole transcript was rendered.
    pub fn lines_window(&self, width: usize, want: usize) -> (Vec<Line<'static>>, bool) {
        let width = width.max(8);
        let mut tail: Vec<Vec<Line<'static>>> = vec![];
        let mut have = 0;
        let mut complete = true;
        for &(a, b) in self.blocks().iter().rev() {
            if have >= want {
                complete = false;
                break;
            }
            let l = self.block_lines(a, b, width);
            have += l.len();
            tail.push(l);
        }
        tail.reverse();
        (tail.into_iter().flatten().collect(), complete)
    }

    fn block_lines(&self, first: usize, end: usize, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let g = self.glyphs;
        let mut out: Vec<Line<'static>> = vec![];
        let row = &self.rows[first];
        // Air between kinds of content: a blank line where the speaker or the kind of row changes.
        let group = row_group(row);
        let prev = first.checked_sub(1).map(|i| row_group(&self.rows[i]));
        if first > 0 && (group == 0 || (matches!(group, 1..=3 | 5) && prev != Some(group))) {
            out.push(Line::default());
        }
        const M: &str = "  ";
        let dim = t.dim;
        match row {
            Row::User(s) => {
                // A card: accent bar on the left, panel background across.
                let card = t.text.patch(t.panel);
                let bar = t.user.patch(t.panel);
                let mut card_line = |text: &str| {
                    let spans = vec![Span::styled(g.user_bar, bar), Span::styled(" ", card), Span::styled(text.to_string(), card)];
                    out.push(fill(spans, width, t.panel));
                };
                if width >= 50 {
                    card_line("");
                }
                for l in wrap(s, width.saturating_sub(3).max(1)) {
                    card_line(&l);
                }
                if width >= 50 {
                    card_line("");
                }
            }
            Row::Assistant(s) => {
                for l in md::render(s, width.saturating_sub(M.len()), t, g) {
                    let mut spans = vec![Span::raw(M)];
                    spans.extend(l.spans.into_iter());
                    out.push(Line::from(spans).style(l.style));
                }
            }
            Row::Reasoning(s) => match self.level {
                0 => {}
                1 => {
                    let lines = s.lines().filter(|l| !l.trim().is_empty()).count();
                    let head = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    let label = format!("thinking{}{} ", if lines > 1 { format!(" ({lines} lines)") } else { String::new() }, "");
                    let room = width.saturating_sub(M.len() + 2 + label.len() + 2);
                    let head = if display_width(&head) > room { format!("{}{}", clip_width(&head, room.saturating_sub(1)), g.ellipsis) } else { head };
                    out.push(Line::from(vec![Span::raw(M), Span::styled(format!("{} ", g.think), t.accent.add_modifier(Modifier::DIM)), Span::styled(label, dim.add_modifier(Modifier::ITALIC)), Span::styled(head, dim.add_modifier(Modifier::ITALIC))]));
                }
                _ => {
                    for l in wrap(s.trim(), width.saturating_sub(M.len() + 2)) {
                        out.push(Line::from(vec![Span::raw(M), Span::styled(format!("{} ", g.vbar), t.border), Span::styled(l, dim.add_modifier(Modifier::ITALIC))]));
                    }
                }
            },
            Row::Tool { depth, agent, .. } if *depth > 0 && self.level < 2 => {
                // A run of a subagent's calls becomes one line (the detail key shows them all).
                let (mut n, mut failed, mut running) = (0, 0, false);
                let (mut last_name, mut last_summary) = (&String::new(), &String::new());
                for r in &self.rows[first..end] {
                    if let Row::Tool { name, summary, state, .. } = r {
                        n += 1;
                        failed += matches!(state, ToolState::Failed(_)) as usize;
                        running |= matches!(state, ToolState::Running);
                        (last_name, last_summary) = (name, summary);
                    }
                }
                let pad = "  ".repeat(*depth);
                let mut text = format!("{agent} {} {n} tool call{}", g.sep, if n == 1 { "" } else { "s" });
                if failed > 0 {
                    text.push_str(&format!(" {} {failed} failed", g.sep));
                }
                text.push_str(&format!(" {} {}: {last_name} {last_summary}", g.sep, if running { "now" } else { "last" }));
                let room = width.saturating_sub(M.len() + pad.len() + 2);
                let text = clip_width(&text, room);
                out.push(Line::from(vec![Span::raw(format!("{M}{pad}")), Span::styled(format!("{} ", if running { g.active } else { g.done }), if failed > 0 { t.error } else { t.accent }), Span::styled(text, if failed > 0 { t.error } else { dim })]));
            }
            Row::Tool { depth, agent, name, summary, state, detail, output } => {
                let pad = "  ".repeat(*depth);
                let glyph = match state {
                    ToolState::Running => Span::styled(if (self.tick / 3) % 2 == 0 { g.run } else { g.idle }, t.accent),
                    ToolState::Done => Span::styled(g.ok, t.ok),
                    ToolState::Failed(_) => Span::styled(g.fail, t.error),
                };
                let who = if *depth > 0 { format!("{agent} {} ", g.sep) } else { String::new() };
                let head = format!("{who}{name:<6} ");
                let room = width.saturating_sub(M.len() + pad.len() + 2 + display_width(&head));
                let sum = if display_width(summary) > room { format!("{}{}", clip_width(summary, room.saturating_sub(1)), g.ellipsis) } else { summary.clone() };
                out.push(Line::from(vec![Span::raw(format!("{M}{pad}")), glyph, Span::raw(" "), Span::styled(head, t.accent), Span::styled(sum, dim)]));
                let indent = format!("{M}{pad}  ");
                if let (Some(d), true) = (detail, self.level >= 1) {
                    let cap = if self.level >= 2 { usize::MAX } else { DETAIL_LINES };
                    let lines: Vec<&str> = d.lines().collect();
                    for raw in lines.iter().take(cap) {
                        for mut l in diff_line(raw, width.saturating_sub(indent.len() + 1), t, g) {
                            l.spans.insert(0, Span::raw(indent.clone()));
                            out.push(l);
                        }
                    }
                    if lines.len() > cap {
                        out.push(Line::styled(format!("{indent}{} {} more lines (Ctrl-O shows all)", g.ellipsis, lines.len() - cap), dim));
                    }
                }
                if let ToolState::Failed(msg) = state {
                    for l in wrap(msg.lines().next().unwrap_or(""), width.saturating_sub(indent.len() + 8)) {
                        out.push(Line::styled(format!("{indent}failed: {l}"), t.error));
                    }
                } else if self.level >= 2 && !output.is_empty() && detail.is_none() {
                    for l in output.lines().take(12).flat_map(|l| wrap(l, width.saturating_sub(indent.len() + 1))) {
                        out.push(Line::styled(format!("{indent}{l}"), dim));
                    }
                }
            }
            Row::Todos(todos) => {
                for td in todos {
                    let (glyph, gs, text_style) = match td.status.as_str() {
                        "completed" => (g.ok, t.ok, dim),
                        "in_progress" => (g.active, t.accent, t.text.add_modifier(Modifier::BOLD)),
                        "cancelled" => (g.fail, dim, dim),
                        _ => (g.idle, dim, t.text),
                    };
                    for (n, l) in wrap(&td.content, width.saturating_sub(M.len() + 2)).into_iter().enumerate() {
                        out.push(Line::from(vec![Span::raw(M), Span::styled(if n == 0 { format!("{glyph} ") } else { "  ".to_string() }, gs), Span::styled(l, text_style)]));
                    }
                }
            }
            Row::Banner { title, model, agent, cwd } => {
                let inner = width.saturating_sub(6).clamp(20, 64);
                let top = format!("╭{}╮", "─".repeat(inner + 2));
                let bottom = format!("╰{}╯", "─".repeat(inner + 2));
                let (top, bottom) = if !g.ascii { (top, bottom) } else { (format!("+{}+", "-".repeat(inner + 2)), format!("+{}+", "-".repeat(inner + 2))) };
                let side = if g.ascii { "|" } else { "│" };
                let row_of = |spans: Vec<Span<'static>>| {
                    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
                    let mut v = vec![Span::raw(M), Span::styled(format!("{side} "), t.border)];
                    v.extend(spans);
                    v.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
                    v.push(Span::styled(format!(" {side}"), t.border));
                    Line::from(v)
                };
                out.push(Line::from(vec![Span::raw(M), Span::styled(top, t.border)]));
                out.push(row_of(vec![Span::styled(title.clone(), t.accent.add_modifier(Modifier::BOLD))]));
                out.push(row_of(vec![Span::styled(clip_width(&format!("{model} {} agent {agent}", g.sep), inner), t.text)]));
                out.push(row_of(vec![Span::styled(clip_width(cwd, inner), dim)]));
                out.push(Line::from(vec![Span::raw(M), Span::styled(bottom, t.border)]));
                out.push(Line::default());
                out.push(Line::from(vec![
                    Span::raw(M),
                    Span::styled("/", t.accent),
                    Span::styled(" commands  ", dim),
                    Span::styled("@", t.accent),
                    Span::styled(" files  ", dim),
                    Span::styled(self.keymap.label(KeyAction::Palette), t.accent),
                    Span::styled(" palette  ", dim),
                    Span::styled(format!("{} h", self.keymap.label(KeyAction::Leader)), t.accent),
                    Span::styled(" help", dim),
                ]));
            }
            Row::Shell(s) => {
                let card = t.text.patch(t.panel);
                out.push(fill(vec![Span::styled(" ", card)], width, t.panel));
                for l in wrap(s.trim_end(), width.saturating_sub(2).max(1)) {
                    out.push(fill(vec![Span::styled(format!(" {l}"), card)], width, t.panel));
                }
                out.push(fill(vec![Span::styled(" ", card)], width, t.panel));
            }
            Row::Info(s) => out.extend(wrap(s, width.saturating_sub(M.len())).into_iter().map(|l| Line::styled(format!("{M}{l}"), dim))),
            Row::Usage(s) => {
                // A thin rule with the turn's numbers in it.
                let text = format!(" {s} ");
                let fillw = width.saturating_sub(M.len() + 2 + display_width(&text));
                out.push(Line::from(vec![Span::raw(M), Span::styled(g.rule.repeat(2), t.border), Span::styled(clip_width(&text, width.saturating_sub(M.len() + 2)), dim), Span::styled(g.rule.repeat(fillw), t.border)]));
            }
            Row::Error(s) => {
                for (n, l) in wrap(s, width.saturating_sub(M.len() + 2)).into_iter().enumerate() {
                    out.push(Line::from(vec![Span::raw(M), Span::styled(if n == 0 { format!("{} ", g.fail) } else { "  ".to_string() }, t.error), Span::styled(l, t.error)]));
                }
            }
        }
        out
    }

    /// The dialog, form or picker that replaces the composer: (title, lines, cursor inside it, border style).
    fn modal_lines(&self, width: usize, max_h: usize) -> Option<(Line<'static>, Vec<Line<'static>>, Option<(u16, u16)>, Style)> {
        let t = &self.theme;
        let g = self.glyphs;
        let inner_w = width.saturating_sub(4);
        let key = |k: &str| Span::styled(format!("[{k}]"), t.accent.add_modifier(Modifier::BOLD));
        if let Some(d) = self.dialogs.front() {
            let mut v = vec![];
            let card = t.code.patch(t.panel);
            for r in d.resources.iter().take(4) {
                let prefix = if d.action == "shell" { "$ " } else { "" };
                let text = format!(" {prefix}{}", clip_width(r.lines().next().unwrap_or(""), inner_w.saturating_sub(3)));
                v.push(fill(vec![Span::styled(text, card)], inner_w, t.panel));
            }
            if d.resources.len() > 4 {
                v.push(Line::styled(format!("{} {} more", g.ellipsis, d.resources.len() - 4), t.dim));
            }
            match &d.feedback {
                Some(f) => {
                    v.push(Line::from(vec![Span::styled("tell the assistant why (Enter sends, Esc goes back): ", t.dim), Span::raw(f.clone())]));
                    let col = display_width("tell the assistant why (Enter sends, Esc goes back): ") + display_width(f);
                    let row = v.len() as u16 - 1;
                    let title = Line::from(vec![Span::styled(format!(" {} {} ", g.fail, d.action), t.warn.add_modifier(Modifier::BOLD))]);
                    return Some((title, v, Some((col as u16, row)), t.warn));
                }
                None => {
                    let mut first = vec![key("y"), Span::raw(" allow once   "), key("a"), Span::raw(" always")];
                    if !d.save.is_empty() {
                        first.push(Span::styled(format!(" ({})", clip_width(&d.save.join(", "), inner_w.saturating_sub(48).max(12))), t.dim));
                    }
                    let second = vec![key("n"), Span::raw(" reject   "), key("f"), Span::raw(" reject with feedback")];
                    v.push(Line::default());
                    let w = |s: &[Span]| s.iter().map(|x| display_width(&x.content)).sum::<usize>();
                    if w(&first) + 3 + w(&second) <= inner_w {
                        first.push(Span::raw("   "));
                        first.extend(second);
                        v.push(Line::from(first));
                    } else {
                        v.push(Line::from(first));
                        v.push(Line::from(second));
                    }
                }
            }
            let title = Line::from(vec![Span::styled(format!(" {} permission needed: {} ", g.run, d.action), t.warn.add_modifier(Modifier::BOLD))]);
            return Some((title, v, None, t.warn));
        }
        if let Some(f) = self.forms.front() {
            let q = f.q();
            let mut v = vec![];
            let head = if q.header.is_empty() { q.question.clone() } else { format!("{}: {}", q.header, q.question) };
            for l in wrap(&head, inner_w) {
                v.push(Line::styled(l, t.text.add_modifier(Modifier::BOLD)));
            }
            v.push(Line::default());
            for (i, o) in q.options.iter().enumerate() {
                let mark = if q.multiple { if f.checked[i] { "[x] " } else { "[ ] " } } else { "" };
                let cur = f.cur == i;
                let text = clip_width(&format!(" {} {}. {mark}{}{}", if cur { g.select } else { " " }, i + 1, o.label, if o.description.is_empty() { String::new() } else { format!("  {}", o.description) }), inner_w);
                v.push(if cur { fill(vec![Span::styled(text, t.bar_hi)], inner_w, t.bar_hi) } else { Line::styled(text, t.text) });
            }
            let own_cur = f.cur == q.options.len();
            let own = clip_width(&format!(" {} {}. Type your own answer", if own_cur { g.select } else { " " }, q.options.len() + 1), inner_w);
            v.push(if own_cur { fill(vec![Span::styled(own, t.bar_hi)], inner_w, t.bar_hi) } else { Line::styled(own, t.dim) });
            let mut cursor = None;
            v.push(Line::default());
            match &f.typing {
                Some(text) => {
                    let line = format!("answer: {text}");
                    cursor = Some((display_width(&line) as u16, v.len() as u16));
                    v.push(Line::raw(line));
                }
                None => v.push(Line::styled(if q.multiple { "Space toggles, Enter confirms, 1-9 toggles, Esc dismisses" } else { "Enter or 1-9 selects, Esc dismisses" }, t.dim)),
            }
            let title = Line::from(vec![Span::styled(format!(" question {}/{} ", f.qi + 1, f.questions.len()), t.accent.add_modifier(Modifier::BOLD))]);
            return Some((title, v, cursor, t.accent));
        }
        if let Some(p) = &self.picker {
            let vis = p.visible();
            let rows = max_h.saturating_sub(4).clamp(1, 12);
            let first = (p.sel + 1).saturating_sub(rows);
            let label_w = vis.iter().map(|&i| display_width(&p.items[i].label)).max().unwrap_or(0).min(inner_w / 2).min(44);
            let mut v = vec![];
            let prompt = format!("{} ", g.prompt);
            let filter_line = if p.filter.is_empty() { Line::from(vec![Span::styled(prompt.clone(), t.accent), Span::styled("type to filter", t.dim)]) } else { Line::from(vec![Span::styled(prompt.clone(), t.accent), Span::raw(p.filter.clone())]) };
            let cursor = Some(((display_width(&prompt) + display_width(&p.filter)) as u16, 0));
            v.push(filter_line);
            if vis.is_empty() {
                v.push(Line::styled("no match", t.dim));
            }
            for (n, &i) in vis.iter().enumerate().skip(first).take(rows) {
                let it = &p.items[i];
                let label = clip_width(&it.label, label_w);
                let pad = " ".repeat(label_w.saturating_sub(display_width(&label)));
                let sel = n == p.sel;
                let line = format!(" {} {label}{pad}  {}", if sel { g.select } else { " " }, it.detail);
                let line = clip_width(&line, inner_w);
                v.push(if sel { fill(vec![Span::styled(line, t.bar_hi)], inner_w, t.bar_hi) } else { Line::from(vec![Span::styled(clip_width(&format!(" {} {label}{pad}", " "), inner_w), t.text), Span::styled(clip_width(&format!("  {}", it.detail), inner_w.saturating_sub(label_w + 4)), t.dim)]) });
            }
            let title = Line::from(vec![Span::styled(format!(" {} {} ", p.title, g.sep), t.accent.add_modifier(Modifier::BOLD)), Span::styled(format!("{} ", vis.len()), t.dim)]);
            return Some((title, v, cursor, t.accent));
        }
        None
    }

    /// The right-hand panel: session, context, todos, MCP servers, jobs, waiting prompts.
    fn sidebar_lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let g = self.glyphs;
        let w = width.saturating_sub(1);
        let head = |s: &str| Line::from(vec![Span::styled(format!(" {} ", s.to_uppercase()), t.dim.add_modifier(Modifier::BOLD)), Span::styled(g.rule.repeat(w.saturating_sub(s.len() + 3)), t.border)]);
        let mut v: Vec<Line<'static>> = vec![];
        for l in wrap(self.title.as_deref().unwrap_or("(untitled session)"), w.saturating_sub(1)).into_iter().take(2) {
            v.push(Line::styled(format!(" {l}"), t.text.add_modifier(Modifier::BOLD)));
        }
        v.push(Line::styled(format!(" {}", clip_width(&self.model, w.saturating_sub(1))), t.dim));
        v.push(Line::styled(format!(" agent {}", self.agent), t.dim));
        v.push(Line::default());
        if self.context > 0 {
            let pct = match self.limit {
                Some(l) if l > 0 => (self.context * 100 / l).min(100) as usize,
                _ => 0,
            };
            let mut spans = vec![Span::styled(" context ", t.dim)];
            if self.limit.is_some() {
                spans.extend(self.gauge(pct, w.saturating_sub(16).clamp(4, 14), Style::new()));
                spans.push(Span::raw(format!(" {pct}%")));
            } else {
                spans.push(Span::raw(human(self.context)));
            }
            v.push(Line::from(spans));
        }
        if self.session_cost > 0.0 {
            v.push(Line::from(vec![Span::styled(" cost    ", t.dim), Span::raw(format!("${:.4}", self.session_cost))]));
        }
        if !self.todos.is_empty() {
            v.push(Line::default());
            v.push(head("todo"));
            for td in &self.todos {
                let (glyph, gs, ts) = match td.status.as_str() {
                    "completed" => (g.ok, t.ok, t.dim),
                    "in_progress" => (g.active, t.accent, t.text.add_modifier(Modifier::BOLD)),
                    "cancelled" => (g.fail, t.dim, t.dim),
                    _ => (g.idle, t.dim, t.text),
                };
                for (n, l) in wrap(&td.content, w.saturating_sub(4)).into_iter().take(2).enumerate() {
                    v.push(Line::from(vec![Span::styled(format!(" {} ", if n == 0 { glyph } else { " " }), gs), Span::styled(l, ts)]));
                }
            }
        }
        if !self.mcp.is_empty() {
            v.push(Line::default());
            v.push(head("mcp"));
            for (name, state) in &self.mcp {
                let ok = state.starts_with("connected");
                v.push(Line::from(vec![Span::styled(format!(" {} ", if ok { g.done } else { g.idle }), if ok { t.ok } else { t.error }), Span::styled(clip_width(&format!("{name}: {state}"), w.saturating_sub(3)), if ok { t.text } else { t.error })]));
            }
        }
        if !self.job_list.is_empty() {
            v.push(Line::default());
            v.push(head("jobs"));
            for (id, what) in &self.job_list {
                v.push(Line::from(vec![Span::styled(format!(" {} ", g.run), t.accent), Span::raw(clip_width(&format!("{id} {what}"), w.saturating_sub(3)))]));
            }
        }
        if !self.queued.is_empty() {
            v.push(Line::default());
            v.push(head("waiting"));
            for q in self.queued.iter().take(4) {
                v.push(Line::from(vec![Span::styled(format!(" {} ", g.next), t.accent), Span::styled(clip_width(q.text.lines().next().unwrap_or(""), w.saturating_sub(4)), t.dim)]));
            }
        }
        v
    }

    /// A bar of `width` cells filled to `pct` percent, colored by how full it is.
    fn gauge(&self, pct: usize, width: usize, bg: Style) -> Vec<Span<'static>> {
        let t = &self.theme;
        let g = self.glyphs;
        let filled = (pct * width).div_ceil(100).min(width);
        let tone = if pct >= 85 { t.error } else if pct >= 60 { t.warn } else { t.ok };
        vec![Span::styled(g.gauge_full.repeat(filled), tone.patch(bg)), Span::styled(g.gauge_empty.repeat(width - filled), t.border.patch(bg))]
    }

    /// The bar along the bottom: model, agent, title, context gauge, cost, jobs on the left, state on the right.
    /// What does not fit is dropped, least important first.
    fn status_line(&self, width: usize) -> Line<'static> {
        let t = &self.theme;
        let g = self.glyphs;
        let bar = t.bar;
        let spin = ["|", "/", "-", "\\"][(self.tick % 4) as usize];
        // (priority, spans): a higher number is dropped first.
        let mut segs: Vec<(u8, Vec<Span<'static>>)> = vec![
            (0, vec![Span::styled(format!(" {} ", self.model), t.bar_hi)]),
            (0, vec![Span::styled(format!(" {} ", self.agent), bar.add_modifier(Modifier::BOLD))]),
        ];
        if let Some(title) = &self.title {
            segs.push((4, vec![Span::styled(format!(" {} ", clip_width(title, 36)), bar.add_modifier(Modifier::DIM))]));
        }
        if self.context > 0 {
            let mut spans = vec![Span::styled(" ctx ", bar)];
            match self.limit {
                Some(l) if l > 0 => {
                    let pct = (self.context * 100 / l).min(100) as usize;
                    spans.extend(self.gauge(pct, 8, bar));
                    spans.push(Span::styled(format!(" {pct}% "), bar));
                }
                _ => spans.push(Span::styled(format!("{} ", human(self.context)), bar)),
            }
            segs.push((2, spans));
        }
        if self.session_cost > 0.0 {
            segs.push((3, vec![Span::styled(format!(" ${:.4} ", self.session_cost), bar)]));
        }
        if self.jobs > 0 {
            segs.push((1, vec![Span::styled(format!(" {} {} job{} (/jobs) ", g.run, self.jobs, if self.jobs == 1 { "" } else { "s" }), t.accent.patch(bar))]));
        }
        let (right, right_style) = if let Some(h) = &self.hint {
            (h.clone(), bar)
        } else if let (true, Some((at, attempt, _))) = (self.running, &self.retry) {
            let left = at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
            (format!("{spin} retrying in {left}s (attempt {attempt}), {} stops", self.keymap.label(KeyAction::Interrupt)), t.warn.patch(bar))
        } else if self.running {
            let text = if width >= 96 { format!("{spin} running: Enter steers, {} queues, {} stops", self.keymap.label(KeyAction::Queue), self.keymap.label(KeyAction::Interrupt)) } else { format!("{spin} running, {} stops", self.keymap.label(KeyAction::Interrupt)) };
            (text, t.accent.patch(bar))
        } else if self.scroll > 0 {
            (format!("scrolled ({} follows)", self.keymap.label(KeyAction::Bottom)), t.warn.patch(bar))
        } else if width >= 80 {
            (format!("Enter sends, {} commands, {} quits", self.keymap.label(KeyAction::Palette), self.keymap.label(KeyAction::Quit)), bar.add_modifier(Modifier::DIM))
        } else {
            (format!("{} commands", self.keymap.label(KeyAction::Palette)), bar.add_modifier(Modifier::DIM))
        };
        let right_w = display_width(&right) + 2;
        let total = |segs: &Vec<(u8, Vec<Span<'static>>)>| -> usize { segs.iter().flat_map(|(_, s)| s.iter()).map(|s| display_width(&s.content)).sum::<usize>() };
        while total(&segs) + right_w > width {
            match segs.iter().enumerate().filter(|(_, (p, _))| *p > 0).max_by_key(|(_, (p, _))| *p).map(|(i, _)| i) {
                Some(i) => {
                    segs.remove(i);
                }
                None => break,
            }
        }
        let mut spans: Vec<Span<'static>> = segs.into_iter().flat_map(|(_, s)| s).collect();
        let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        let room = width.saturating_sub(used + 2);
        let right = clip_width(&right, room);
        spans.push(Span::styled(" ".repeat(room.saturating_sub(display_width(&right)) + 1), bar));
        spans.push(Span::styled(right, right_style));
        spans.push(Span::styled(" ", bar));
        Line::from(spans).style(bar)
    }
}

fn row_group(r: &Row) -> u8 {
    match r {
        Row::User(_) => 0,
        Row::Assistant(_) => 1,
        Row::Tool { .. } | Row::Reasoning(_) => 2,
        Row::Usage(_) => 3,
        Row::Todos(_) => 5,
        _ => 4,
    }
}

/// Pad a line with spaces in `style` out to `width` (so a background reaches the right edge).
fn fill(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Line<'static> {
    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(spans)
}

/// One line of a unified diff. With truecolor the added and removed lines get a background that runs to the
/// right edge; otherwise only the `+`/`-` gutter has one and the rest is colored text.
fn diff_line(raw: &str, width: usize, t: &Theme, _g: &Glyphs) -> Vec<Line<'static>> {
    let width = width.max(8);
    let first = raw.chars().next();
    let kind = match first {
        Some('+') if !raw.starts_with("+++") => Some(t.added),
        Some('-') if !raw.starts_with("---") => Some(t.removed),
        _ => None,
    };
    if let Some(style) = kind {
        let lines = wrap(raw, width);
        return lines
            .into_iter()
            .enumerate()
            .map(|(n, l)| {
                if t.diff_fill {
                    fill(vec![Span::styled(l, style)], width, style)
                } else {
                    // The gutter keeps the background; the text only the foreground.
                    let text_style = Style { bg: None, ..style };
                    if n == 0 {
                        let rest: String = l.chars().skip(1).collect();
                        Line::from(vec![Span::styled(first.unwrap_or(' ').to_string(), style.add_modifier(Modifier::BOLD)), Span::styled(rest, text_style)])
                    } else {
                        Line::styled(l, text_style)
                    }
                }
            })
            .collect();
    }
    let style = match first {
        Some('@') | Some('*') => t.accent.add_modifier(Modifier::DIM),
        _ => t.dim,
    };
    wrap(raw, width).into_iter().map(|l| Line::styled(l, style)).collect()
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

fn render_viewer(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (bar, hi, accent, dim) = (app.theme.bar, app.theme.bar_hi, app.theme.accent.patch(app.theme.bar), app.theme.dim);
    let h = parts[1].height as usize;
    app.view_h = h;
    app.comp_w = (area.width as usize).saturating_sub(2);
    let (theme, g) = (app.theme.clone(), app.glyphs);
    let Some(v) = app.viewer.as_mut() else { return };
    let (lines, starts) = v.lines(area.width as usize, &theme, g);
    v.scroll = v.scroll.min(lines.len().saturating_sub(h));
    let (adds, dels): (usize, usize) = v.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
    let at_end = v.scroll + h >= lines.len();
    let cur = if at_end { v.files.len() } else { starts.iter().rposition(|&s| s <= v.scroll).map_or(0, |i| i + 1) };
    let head = vec![
        Span::styled(" changes ", hi),
        Span::styled(format!(" {} file{}  ", v.files.len(), if v.files.len() == 1 { "" } else { "s" }), bar),
        Span::styled(format!("+{adds}"), theme.added.fg.map_or(bar, |c| bar.fg(c))),
        Span::styled(" ", bar),
        Span::styled(format!("-{dels}"), theme.removed.fg.map_or(bar, |c| bar.fg(c))),
        Span::styled(" since the session started", bar.add_modifier(Modifier::DIM)),
    ];
    f.render_widget(Paragraph::new(Line::from(head)).style(bar), parts[0]);
    f.render_widget(Paragraph::new(lines.iter().skip(v.scroll).take(h).cloned().collect::<Vec<_>>()), parts[1]);
    let at = if lines.len() <= h { "all".to_string() } else { format!("{}%", (v.scroll + h).min(lines.len()) * 100 / lines.len()) };
    let key = |k: &str, what: &str| vec![Span::styled(format!(" {k}"), accent), Span::styled(format!(" {what} "), bar.add_modifier(Modifier::DIM))];
    let mut foot = vec![];
    for (k, what) in [("j/k", "scroll"), ("Space", "page"), ("n/p", "file"), ("q", "close")] {
        foot.extend(key(k, what));
    }
    foot.push(Span::styled(format!("  file {cur}/{}  {at}", v.files.len()), bar));
    let _ = dim;
    f.render_widget(Paragraph::new(Line::from(foot)).style(bar), parts[2]);
}

pub fn render(f: &mut Frame, app: &mut App) {
    if app.viewer.is_some() && app.dialogs.is_empty() && app.forms.is_empty() {
        return render_viewer(f, app);
    }
    let t = app.theme.clone();
    let g = app.glyphs;
    let full = f.area();
    // The sidebar needs room: at least 76 columns in all, and it is shown by default from 110.
    let fits = full.width >= 76;
    app.side_fits = Some(fits);
    app.side_visible = app.sidebar.unwrap_or(full.width >= 110) && fits;
    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(full);
    let (body, status) = (rows[0], rows[1]);
    let (area, side) = if app.side_visible {
        let c = Layout::horizontal([Constraint::Min(40), Constraint::Length(SIDEBAR_W)]).split(body);
        (c[0], Some(c[1]))
    } else {
        (body, None)
    };
    if let Some(side) = side {
        let block = Block::new().borders(Borders::LEFT).border_set(g.border).border_style(t.border);
        let inner = block.inner(side);
        f.render_widget(Paragraph::new(app.sidebar_lines(inner.width as usize)).block(block), side);
    }
    let max_modal = (area.height as usize).saturating_sub(5).max(3);
    let modal = app.modal_lines(area.width as usize, max_modal);
    // Inside the box: two columns for the prompt marker.
    let comp_w = (area.width as usize).saturating_sub(4).max(1);
    app.comp_w = comp_w;
    let comp_lines = app.layout(comp_w);
    let visible = comp_lines.len().min(MAX_COMPOSER_LINES);
    let bottom_h = modal.as_ref().map_or(visible as u16 + 2, |m| m.1.len() as u16 + 2).min(area.height.saturating_sub(2).max(1));
    // Prompts waiting for delivery sit between the transcript and the composer.
    let shown_q = app.queued.len().min(3);
    let strip_h = (shown_q + (app.queued.len() > 3) as usize).min((area.height as usize).saturating_sub(6)) as u16;
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(strip_h), Constraint::Length(bottom_h)]).split(area);
    let (main, strip, bottom) = (parts[0], parts[1], parts[2]);
    if strip_h > 0 {
        let mut ql: Vec<Line> = app
            .queued
            .iter()
            .take(shown_q)
            .map(|q| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} ", g.next), t.accent),
                    Span::styled(if q.steer { "next step " } else { "after turn " }, t.dim),
                    Span::styled(clip_width(q.text.lines().next().unwrap_or(""), (area.width as usize).saturating_sub(20)), t.text),
                ])
            })
            .collect();
        if app.queued.len() > 3 {
            ql.push(Line::styled(format!("    +{} more (/queue)", app.queued.len() - 3), t.dim));
        }
        ql.truncate(strip_h as usize);
        f.render_widget(Paragraph::new(ql), strip);
    }

    let h = main.height as usize;
    app.view_h = h;
    let (lines, complete) = app.lines_window(main.width as usize, app.scroll.saturating_add(h));
    if complete {
        app.scroll = app.scroll.min(lines.len().saturating_sub(h));
    }
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(h);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), main);

    if let Some((title, lines, cursor, border)) = modal {
        let block = Block::bordered().border_set(g.border).title(title).border_style(border).padding(Padding::horizontal(1));
        let inner = block.inner(bottom);
        f.render_widget(Paragraph::new(lines).block(block), bottom);
        if let Some((col, row)) = cursor {
            f.set_cursor_position((inner.x + col.min(inner.width.saturating_sub(1)), inner.y + row));
        }
    } else {
        let chars: Vec<char> = app.input.chars().collect();
        let (title, border) = if app.running {
            (Line::from(vec![Span::styled(format!(" {} working ", g.run), t.accent)]), t.border)
        } else {
            (Line::from(vec![Span::styled(" message ", t.accent.add_modifier(Modifier::BOLD))]), t.accent)
        };
        let mut block = Block::bordered().border_set(g.border).title(title).border_style(border);
        if app.input.is_empty() && !app.running {
            let hint = format!(" Enter send {} {} newline ", g.sep, app.keymap.label(KeyAction::Newline));
            block = block.title_bottom(Line::from(Span::styled(hint, t.dim)).alignment(Alignment::Right));
        }
        let inner = block.inner(bottom);
        let (row, col) = app.cursor_pos(comp_w);
        // Show the window of lines that contains the cursor.
        let shown = (inner.height as usize).max(1);
        let top = (row + 1).saturating_sub(shown);
        let mut rows: Vec<Line> = vec![];
        for (n, &(a, b)) in comp_lines.iter().enumerate().skip(top).take(shown) {
            let marker = if n == 0 { Span::styled(format!("{} ", g.prompt), t.accent.add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
            let text: String = chars[a..b].iter().collect();
            if app.input.is_empty() && n == 0 && !app.running {
                rows.push(Line::from(vec![marker, Span::styled("Ask anything. / for commands, @ to mention a file", t.dim)]));
            } else {
                rows.push(Line::from(vec![marker, Span::raw(text)]));
            }
        }
        f.render_widget(Paragraph::new(rows).block(block), bottom);
        f.set_cursor_position((inner.x + 2 + (col as u16).min(inner.width.saturating_sub(3)), inner.y + (row - top) as u16));
    }
    if let Some(p) = &app.popup {
        let n = p.items.len().min(MAX_POPUP) as u16;
        let width = main.width.min(74);
        let h = (n + 2).min(main.height);
        let rect = Rect::new(main.x + 1, main.y + main.height - h, width.saturating_sub(1), h);
        let first = (p.sel + 1).saturating_sub(MAX_POPUP);
        let inner_w = rect.width.saturating_sub(2) as usize;
        let label_w = p.items.iter().map(|i| display_width(&i.label)).max().unwrap_or(0).min(inner_w / 2);
        let lines: Vec<Line> = p
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(MAX_POPUP)
            .map(|(i, it)| {
                let label = clip_width(&it.label, label_w);
                let pad = " ".repeat(label_w.saturating_sub(display_width(&label)));
                if i == p.sel {
                    let text = clip_width(&format!(" {label}{pad}  {}", it.detail), inner_w);
                    fill(vec![Span::styled(text, t.bar_hi)], inner_w, t.bar_hi)
                } else {
                    let detail = clip_width(&it.detail, inner_w.saturating_sub(label_w + 3));
                    fill(vec![Span::styled(format!(" {label}{pad}"), t.accent.patch(t.panel)), Span::styled(format!("  {detail}"), t.dim.patch(t.panel))], inner_w, t.panel)
                }
            })
            .collect();
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(lines).block(Block::bordered().border_set(g.border).border_style(t.border)), rect);
    }
    f.render_widget(Paragraph::new(app.status_line(status.width as usize)).style(t.bar), status);
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
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Enqueue { text: "more".into(), shown: "more".into(), queue: false });
        type_str(&mut a, "more");
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
        text(&mut a, Ev::Tool { name: "read".into(), summary: "a.rs".into(), detail: None });
        text(&mut a, Ev::Tool { name: "read".into(), summary: "b.rs".into(), detail: None });
        text(&mut a, Ev::ToolDone { name: "read".into(), ok: false, msg: "no such file\nmore".into(), output: String::new() });
        text(&mut a, Ev::Text("done".into()));
        assert_eq!(a.rows.len(), 5);
        let states: Vec<_> = a.rows.iter().filter_map(|r| if let Row::Tool { state, .. } = r { Some(state.clone()) } else { None }).collect();
        assert_eq!(states, vec![ToolState::Running, ToolState::Failed("no such file\nmore".into())]); // the newest running call finishes first
        let s = screen(&mut a, 40, 12);
        let joined = s.join("\n");
        assert!(joined.contains("hi") && joined.contains("Hello") && joined.contains("read   a.rs") && joined.contains("failed: no such file"), "{joined}");
        // Subagent prose is hidden, its tools are indented.
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Text("secret".into()) });
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Tool { name: "grep".into(), summary: "x".into(), detail: None } });
        let joined = screen(&mut a, 60, 14).join("\n");
        assert!(!joined.contains("secret") && joined.contains("explore · 1 tool call · now: grep x"), "{joined}");
    }

    #[test]
    fn scrolling_follows_the_tail_until_the_user_scrolls_up() {
        let mut a = App::new("m", "build");
        for i in 0..30 {
            a.push_info(format!("line {i}"));
        }
        let s = screen(&mut a, 30, 10); // 10 - 3 composer - 1 status = 6 transcript lines
        assert_eq!(s[5], "  line 29");
        press(&mut a, KeyCode::PageUp);
        let s = screen(&mut a, 30, 10);
        assert_eq!(s[5], "  line 24"); // one page (view - 1 = 5 lines) up
        assert!(s[9].contains("scrolled"));
        a.update(key(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(screen(&mut a, 30, 10)[5], "  line 29");
        a.update(key(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(screen(&mut a, 30, 10)[0], "  line 0"); // clamped to the top
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
        assert!(joined.contains("permission needed: shell") && joined.contains("rm -rf x") && joined.contains("[a] always (rm *)"), "{joined}");
        type_str(&mut a, "a");
        assert_eq!(rx1.try_recv().unwrap(), Reply::Always);
        assert!(screen(&mut a, 70, 12).join("\n").contains("permission needed: edit")); // the next one is shown
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
        assert!(joined.contains("interrupted") && joined.contains("1200 in · 80 out · 4000 cached · $0.0123 · session $0.0500"), "{joined}");
        assert!(joined.contains("$0.0500") && joined.contains("ctx ") && joined.contains(" 5%"), "{joined}");
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
        screen(&mut a, 14, 12); // composer inner width 10 (border and prompt marker take four columns)
        type_str(&mut a, "abcdefghijklmno"); // wraps after 10 chars
        assert_eq!(a.layout(10), vec![(0, 10), (10, 15)]);
        assert_eq!(a.cursor_pos(10), (1, 5));
        press(&mut a, KeyCode::Up);
        assert_eq!(a.cursor, 5);
        press(&mut a, KeyCode::Down);
        assert_eq!(a.cursor, 15);
        let s = screen(&mut a, 14, 12);
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
        assert!(joined.contains(" hello") && joined.contains(" world"), "{joined}");
        // The composer grows with its content up to a limit.
        for _ in 0..20 {
            a.update(key(KeyCode::Enter, KeyModifiers::ALT));
        }
        let s = screen(&mut a, 30, 20);
        assert_eq!(s.iter().filter(|l| l.starts_with('│')).count(), 8, "{s:?}");
    }

    fn q(question: &str, opts: &[&str], multiple: bool) -> Question {
        Question {
            question: question.into(),
            header: "H".into(),
            options: opts.iter().map(|o| moco_core::tools::QuestionOption { label: o.to_string(), description: String::new() }).collect(),
            multiple,
        }
    }

    #[test]
    fn question_form_collects_single_multiple_and_typed_answers() {
        let mut a = App::new("m", "build");
        a.running = true;
        let (tx, rx) = channel();
        a.update(AppEvent::Questions { questions: vec![q("Which?", &["red", "green"], false), q("Pick some", &["a", "b", "c"], true), q("Name?", &["x"], false)], reply: tx });
        let joined = screen(&mut a, 60, 16).join("\n");
        assert!(joined.contains("question 1/3") && joined.contains("H: Which?") && joined.contains("1. red") && joined.contains("Type your own answer"), "{joined}");
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter); // single: Enter picks the highlighted option
        type_str(&mut a, "x"); // letters mean nothing in option mode
        press(&mut a, KeyCode::Char(' ')); // multiple: toggle a (cursor on a), move, toggle c
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Char(' '));
        assert!(screen(&mut a, 60, 16).join("\n").contains("3. [x] c"));
        press(&mut a, KeyCode::Enter);
        // Third question: "type your own" is the last row.
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        type_str(&mut a, "Ada");
        press(&mut a, KeyCode::Backspace);
        type_str(&mut a, "a");
        assert!(rx.try_recv().is_err()); // nothing is sent before the last answer
        press(&mut a, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), Some(vec![vec!["green".to_string()], vec!["a".into(), "c".into()], vec!["Ada".into()]]));
        assert!(a.forms.is_empty());
        // Digits answer a single-choice question at once; Esc dismisses (the sender is dropped: None for the engine).
        let (tx, rx) = channel();
        a.update(AppEvent::Questions { questions: vec![q("Which?", &["red", "green"], false)], reply: tx });
        type_str(&mut a, "2");
        assert_eq!(rx.try_recv().unwrap(), Some(vec![vec!["green".to_string()]]));
        let (tx, rx) = channel();
        a.update(AppEvent::Questions { questions: vec![q("Which?", &["red"], false)], reply: tx });
        press(&mut a, KeyCode::Esc);
        assert!(rx.recv().is_err());
    }

    fn items(names: &[&str]) -> Vec<PickItem> {
        names.iter().map(|n| PickItem { label: n.to_string(), detail: format!("about {n}"), value: format!("v-{n}") }).collect()
    }

    #[test]
    fn picker_filters_selects_and_closes() {
        let mut a = App::new("m", "build");
        a.open_picker(Picker::new(PickKind::Model, "model", items(&["anthropic/sonnet", "openai/gpt", "anthropic/haiku"]), ""));
        let joined = screen(&mut a, 70, 14).join("\n");
        assert!(joined.contains("model · 3") && joined.contains("anthropic/sonnet") && joined.contains("about openai/gpt"), "{joined}");
        type_str(&mut a, "anth hai"); // every word must match, in label or detail
        assert_eq!(a.picker.as_ref().unwrap().visible(), vec![2]);
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Picked { kind: PickKind::Model, value: "v-anthropic/haiku".into() });
        assert!(a.picker.is_none());
        // Typing never reaches the composer while a picker is open; Esc closes without a choice; no match picks nothing.
        a.open_picker(Picker::new(PickKind::Session, "sessions", items(&["one", "two"]), ""));
        type_str(&mut a, "zzz");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::None);
        assert!(a.picker.is_some() && a.input.is_empty());
        press(&mut a, KeyCode::Esc);
        assert!(a.picker.is_none());
        // Selection wraps around and the list scrolls to keep it visible.
        let many: Vec<String> = (0..30).map(|i| format!("item{i:02}")).collect();
        a.open_picker(Picker::new(PickKind::Command, "commands", items(&many.iter().map(String::as_str).collect::<Vec<_>>()), ""));
        press(&mut a, KeyCode::Up);
        assert_eq!(a.picker.as_ref().unwrap().sel, 29);
        assert!(screen(&mut a, 60, 20).join("\n").contains("item29"));
        assert_eq!(press(&mut a, KeyCode::Enter), Action::Picked { kind: PickKind::Command, value: "v-item29".into() });
        // Ctrl-P opens the palette from the composer.
        assert_eq!(a.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL)), Action::Palette);
    }

    #[test]
    fn assistant_text_is_rendered_as_markdown() {
        let mut a = App::new("m", "build");
        text(&mut a, Ev::Text("## Plan\n\n- fix `foo`\n- **ship**\n\n```rust\nlet x = 1;\n```\n".into()));
        let joined = screen(&mut a, 40, 14).join("\n");
        assert!(joined.contains("Plan\n") && !joined.contains("##") && joined.contains("• fix foo") && joined.contains("• ship") && joined.contains("  let x = 1;") && !joined.contains("```"), "{joined}");
        // Streaming: a reply cut in the middle of a marker renders as plain text, then fixes itself.
        let mut a = App::new("m", "build");
        text(&mut a, Ev::Text("use **bol".into()));
        assert!(screen(&mut a, 40, 10).join("\n").contains("use **bol"));
        text(&mut a, Ev::Text("d** now".into()));
        assert!(screen(&mut a, 40, 10).join("\n").contains("use bold now"));
    }

    fn tool_with_detail(a: &mut App, name: &str, detail: &str) {
        text(a, Ev::Tool { name: name.into(), summary: "x.rs".into(), detail: Some(detail.into()) });
        text(a, Ev::ToolDone { name: name.into(), ok: true, msg: String::new(), output: "line one\nline two".into() });
    }

    #[test]
    fn file_changes_show_their_diff_inline_by_detail_level() {
        let mut a = App::new("m", "build");
        let many: String = (0..40).map(|i| format!("+added {i}")).collect::<Vec<_>>().join("\n");
        tool_with_detail(&mut a, "edit", "@@ -1 +1 @@\n-old line\n+new line");
        tool_with_detail(&mut a, "write", &many);
        let s = screen(&mut a, 60, 50).join("\n");
        assert!(s.contains("edit   x.rs") && s.contains("-old line") && s.contains("+new line") && s.contains("+added 23") && !s.contains("+added 24") && s.contains("… 16 more lines (Ctrl-O shows all)"), "{s}");
        let lines = a.lines_window(60, usize::MAX).0;
        // Without truecolor the +/- gutter carries the background and the text only the color.
        let find = |needle: &str| lines.iter().find(|l| l.spans.iter().any(|s| s.content.contains(needle))).unwrap().spans.clone();
        let (old, new) = (find("old line"), find("new line"));
        assert_eq!((old[1].content.as_ref(), old[1].style.bg, old[2].style.fg, old[2].style.bg), ("-", a.theme.removed.bg, a.theme.removed.fg, None));
        assert_eq!((new[1].content.as_ref(), new[1].style.bg, new[2].style.fg), ("+", a.theme.added.bg, a.theme.added.fg));
        // Level 2 (Ctrl-O): everything, plus tool output where there is no diff.
        a.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(a.level, 2);
        text(&mut a, Ev::Tool { name: "grep".into(), summary: "q".into(), detail: None });
        text(&mut a, Ev::ToolDone { name: "grep".into(), ok: true, msg: String::new(), output: "hit one\nhit two".into() });
        let s = screen(&mut a, 60, 70).join("\n");
        assert!(s.contains("+added 39") && !s.contains("more lines") && s.contains("  hit one"), "{s}");
        // Level 0: just the call lines.
        a.update(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(a.level, 0);
        let s = screen(&mut a, 60, 50).join("\n");
        assert!(s.contains("edit   x.rs") && !s.contains("-old line") && !s.contains("+added"), "level 0 shows only the call lines: {s}");
    }

    #[test]
    fn reasoning_is_summarized_hidden_or_shown() {
        let mut a = App::new("m", "build");
        text(&mut a, Ev::Reasoning("Let me think\nabout the *edge* case".into()));
        text(&mut a, Ev::Reasoning(" more".into()));
        text(&mut a, Ev::Text("Answer".into()));
        assert_eq!(a.rows.len(), 2); // reasoning chunks join into one row
        let s = screen(&mut a, 70, 10).join("\n");
        assert!(s.contains("thinking (2 lines) Let me think about the *edge* case more") && s.contains("Answer"), "{s}");
        a.level = 2;
        let s = screen(&mut a, 40, 10).join("\n");
        assert!(s.contains("Let me think") && s.contains("about the *edge* case more") && !s.contains("thinking ("), "{s}");
        a.level = 0;
        let s = screen(&mut a, 40, 10).join("\n");
        assert!(!s.contains("Let me think") && s.contains("Answer"), "{s}");
        // Subagent reasoning stays hidden.
        a.update(AppEvent::Engine { depth: 1, agent: "x".into(), ev: Ev::Reasoning("secret".into()) });
        a.level = 2;
        assert!(!screen(&mut a, 40, 10).join("\n").contains("secret"));
    }

    fn file(path: &str, patch: &str) -> ViewFile {
        ViewFile { path: path.into(), adds: patch.lines().filter(|l| l.starts_with('+')).count(), dels: patch.lines().filter(|l| l.starts_with('-')).count(), binary: false, patch: patch.into() }
    }

    #[test]
    fn diff_viewer_scrolls_jumps_between_files_and_closes() {
        let mut a = App::new("m", "build");
        let big: String = (0..60).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n");
        a.viewer = Some(Viewer { files: vec![file("a.rs", &big), file("b.rs", "-gone\n+here")], scroll: 0 });
        let s = screen(&mut a, 60, 12);
        assert!(s[0].contains("2 files") && s[0].contains("+61 -1") && s[1].contains("a.rs  +60 -0") && s[2].contains("+line 0") && s[11].contains("file 1/2"), "{s:?}");
        press(&mut a, KeyCode::Char('j'));
        assert!(screen(&mut a, 60, 12)[1].contains("+line 0"), "scrolled one line");
        press(&mut a, KeyCode::Char('n')); // next file
        let s = screen(&mut a, 60, 12);
        assert!(s.iter().any(|l| l.contains("b.rs  +1 -1")) && s[11].contains("file 2/2"), "{s:?}"); // a short last file cannot reach the top
        press(&mut a, KeyCode::Char('p'));
        assert!(screen(&mut a, 60, 12).iter().any(|l| l.contains("+line 5")), "p goes back to the first file's lines");
        press(&mut a, KeyCode::Char('G'));
        assert!(screen(&mut a, 60, 12).iter().any(|l| l.contains("+here")));
        press(&mut a, KeyCode::Char('g'));
        assert!(screen(&mut a, 60, 12)[1].contains("a.rs"));
        // Typing goes to the viewer, not the composer; q closes it.
        press(&mut a, KeyCode::Char('x'));
        assert!(a.input.is_empty());
        press(&mut a, KeyCode::Char('q'));
        assert!(a.viewer.is_none());
        // An engine dialog is shown above the viewer.
        a.viewer = Some(Viewer { files: vec![file("a.rs", "+x")], scroll: 0 });
        let (tx, _rx) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec!["ls".into()], save: vec![], reply: tx });
        assert!(screen(&mut a, 60, 12).join("\n").contains("permission needed: shell"));
    }

    #[test]
    fn a_long_transcript_costs_a_screenful_not_the_whole_history() {
        let mut a = App::new("m", "build");
        for i in 0..20_000 {
            a.rows.push(Row::Assistant(format!("## Heading {i}\n\n- item **{i}** with `code` and a [link](http://x/{i})")));
        }
        let t = std::time::Instant::now();
        let (lines, complete) = a.lines_window(80, 30);
        assert!(!complete && lines.len() >= 30 && lines.len() < 200, "rendered {} lines", lines.len());
        assert!(t.elapsed() < std::time::Duration::from_millis(250), "took {:?}", t.elapsed());
        let s = screen(&mut a, 80, 20).join("\n");
        assert!(s.contains("Heading 19999") && !s.contains("Heading 19000"), "{s}");
        // Scrolling far up renders more, and the top is reachable.
        a.scroll = 10_000;
        assert!(screen(&mut a, 80, 20).join("\n").contains("Heading"));
        a.scroll = usize::MAX / 4;
        let s = screen(&mut a, 80, 20);
        assert!(a.scroll < 200_000 && s.join("\n").contains("Heading 0"), "scroll {}", a.scroll);
    }

    fn ctrl(c: char) -> AppEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn the_leader_key_runs_commands_and_actions() {
        let mut a = App::new("m", "build");
        assert_eq!(a.update(ctrl('x')), Action::None);
        assert!(a.hint.as_deref().unwrap().starts_with("leader: ") && a.hint.as_deref().unwrap().contains("n new") && a.hint.as_deref().unwrap().contains("b sidebar"), "{:?}", a.hint);
        assert_eq!(type_str_last(&mut a, "n"), Action::Submit { text: "/new".into(), shown: "/new".into() });
        assert!(a.input.is_empty(), "the key after the leader is not typed");
        a.update(ctrl('x'));
        assert_eq!(type_str_last(&mut a, "z"), Action::None);
        assert!(a.hint.as_deref().unwrap().contains("does nothing"));
        assert!(a.input.is_empty());
        a.update(ctrl('x'));
        assert_eq!(press(&mut a, KeyCode::Esc), Action::None); // cancels
        type_str(&mut a, "n");
        assert_eq!(a.input, "n");
        a.input.clear();
        a.cursor = 0;
        a.update(ctrl('x'));
        assert_eq!(type_str_last(&mut a, "p"), Action::Palette);
        a.update(ctrl('x'));
        assert_eq!(type_str_last(&mut a, "q"), Action::Submit { text: "/exit".into(), shown: "/exit".into() });
        a.update(ctrl('x'));
        type_str(&mut a, "o");
        assert_eq!(a.level, 2);
    }

    fn type_str_last(a: &mut App, s: &str) -> Action {
        let mut last = Action::None;
        for c in s.chars() {
            last = press(a, KeyCode::Char(c));
        }
        last
    }

    #[test]
    fn configured_keys_replace_the_defaults() {
        let mut a = App::new("m", "build");
        let keys = serde_json::json!({"palette": "ctrl+k", "quit": "ctrl+q", "sidebar": "f3", "page_up": "alt+u"}).as_object().unwrap().clone().into_iter().collect();
        a.keymap = Keymap::from_config(&keys, &Default::default()).0;
        assert_eq!(a.update(ctrl('k')), Action::Palette);
        assert_eq!(a.update(ctrl('p')), Action::None);
        assert_eq!(a.update(ctrl('q')), Action::Quit); // empty composer, idle
        type_str(&mut a, "abc");
        assert_eq!(a.update(ctrl('q')), Action::None); // not with text in the way
        assert_eq!(a.update(ctrl('d')), Action::None); // Ctrl-D is just an edit key now
        assert_eq!(a.input, "abc");
        a.update(key(KeyCode::Char('u'), KeyModifiers::ALT));
        assert!(a.scroll > 0);
        a.update(key(KeyCode::F(3), KeyModifiers::NONE));
        assert_eq!(a.sidebar, Some(true));
        // The status bar names the configured keys.
        a.input.clear();
        a.cursor = 0;
        assert!(screen(&mut a, 90, 8).join("\n").contains("Ctrl-K commands, Ctrl-Q quits"));
    }

    #[test]
    fn the_sidebar_appears_on_wide_terminals_and_shows_session_state() {
        let mut a = App::new("anthropic/sonnet", "build");
        a.title = Some("Fix the parser".into());
        a.session_cost = 0.0123;
        a.context = 50_000;
        a.limit = Some(200_000);
        a.todos = vec![Todo { content: "write tests".into(), status: "in_progress".into() }, Todo { content: "ship".into(), status: "pending".into() }];
        a.mcp = vec![("files".into(), "connected: 3 tool(s)".into()), ("web".into(), "failed: no url".into())];
        a.job_list = vec![("shl_1".into(), "shell 12s".into())];
        a.queued = vec![Queued { id: "i1".into(), steer: true, text: "also this".into() }];
        let wide = screen(&mut a, 120, 24).join("\n");
        for want in ["Fix the parser", "context ━━━━", "25%", "cost    $0.0123", "TODO", "◐ write tests", "MCP", "files: connected", "web: failed", "JOBS", "shl_1 shell 12s", "WAITING", "also this"] {
            assert!(wide.contains(want), "missing {want:?}:\n{wide}");
        }
        assert!(!screen(&mut a, 100, 24).join("\n").contains("TODO"), "narrower than 110: hidden by default");
        a.sidebar = Some(true);
        assert!(screen(&mut a, 100, 24).join("\n").contains("TODO"), "forced on");
        a.sidebar = Some(true);
        assert!(!screen(&mut a, 70, 24).join("\n").contains("TODO"), "too narrow even when forced");
        screen(&mut a, 120, 24); // the toggle flips what was last shown
        a.update(ctrl('x'));
        type_str(&mut a, "b"); // leader + b toggles
        assert_eq!(a.sidebar, Some(false));
        assert!(!screen(&mut a, 120, 24).join("\n").contains("TODO"));
        a.update(ctrl('x'));
        type_str(&mut a, "b");
        assert!(screen(&mut a, 120, 24).join("\n").contains("TODO"));
    }

    #[test]
    fn attention_is_raised_for_finished_turns_and_requests_and_away_follows_focus_or_idleness() {
        let mut a = App::new("m", "build");
        a.running = true;
        a.update(AppEvent::RunDone(Ok(RunOutcome::Idle)));
        assert_eq!(a.attention.take().as_deref(), Some("done"));
        a.update(AppEvent::RunDone(Ok(RunOutcome::Interrupted)));
        assert!(a.attention.is_none(), "you interrupted it yourself");
        let (tx, _rx) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec![], save: vec![], reply: tx });
        assert_eq!(a.attention.take().as_deref(), Some("shell needs permission"));
        let (tx, _rx) = channel();
        a.update(AppEvent::Questions { questions: vec![q("Which?", &["a"], false)], reply: tx });
        assert!(a.attention.take().unwrap().contains("question"));
        // Away: the terminal's focus report wins; without one, idleness decides.
        let mut a = App::new("m", "build");
        a.away_after = std::time::Duration::from_millis(30);
        assert!(!a.away());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(a.away());
        press(&mut a, KeyCode::Char('x'));
        assert!(!a.away(), "a key press means you are here");
        a.update(AppEvent::Focus(false));
        assert!(a.away());
        a.update(AppEvent::Focus(true));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!a.away(), "focused terminals never count as away, however idle");
    }

    #[test]
    fn themes_change_how_rows_are_styled() {
        let mut a = App::new("m", "build");
        a.push_user("hi");
        let fg = |a: &App| a.lines_window(40, 10).0.iter().find(|l| l.spans.iter().any(|s| s.content.contains("hi"))).map(|l| l.spans[0].style.fg);
        assert_eq!(fg(&a), Some(Some(Color::Cyan)));
        a.theme = Theme::light();
        assert_eq!(fg(&a), Some(Some(Color::Blue)));
        a.theme = Theme::mono();
        assert_eq!(fg(&a), Some(None));
    }

    #[test]
    fn ascii_mode_draws_with_ascii_only() {
        let mut a = App::new("m", "build");
        a.glyphs = &crate::tui::glyphs::ASCII;
        a.rows.push(Row::Banner { title: "moco".into(), model: "m".into(), agent: "build".into(), cwd: "~/x".into() });
        a.push_user("hello");
        text(&mut a, Ev::Reasoning("hmm".into()));
        text(&mut a, Ev::Text("# Title\n\n- item\n> quote\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```\ncode\n```".into()));
        text(&mut a, Ev::Tool { name: "edit".into(), summary: "x".into(), detail: Some("@@ -1 +1 @@\n-a\n+b".into()) });
        text(&mut a, Ev::ToolDone { name: "edit".into(), ok: true, msg: String::new(), output: String::new() });
        text(&mut a, Ev::Todos(vec![Todo { content: "t".into(), status: "completed".into() }]));
        text(&mut a, Ev::Usage { usage: Usage { input: 1, output: 1, cache_read: 0, cache_write: 0 }, cost: Some(0.1), session_cost: 0.1, context: 10, limit: Some(100) });
        a.running = true;
        a.queued.push(Queued { id: "q".into(), steer: true, text: "later".into() });
        a.jobs = 1;
        a.update(AppEvent::RunDone(Ok(RunOutcome::Idle)));
        a.sidebar = Some(true);
        let s = screen(&mut a, 120, 40).join("\n");
        let odd: Vec<char> = s.chars().filter(|c| !c.is_ascii() && *c != '\n').collect();
        assert!(odd.is_empty(), "non-ASCII characters in ASCII mode: {odd:?}\n{s}");
    }

    #[test]
    fn the_cursor_stays_visible_in_a_long_line() {
        let mut a = App::new("m", "build");
        type_str(&mut a, &"x".repeat(50));
        type_str(&mut a, "END");
        let s = screen(&mut a, 20, 8);
        assert!(s[s.len() - 3].contains("xxEND"), "{s:?}"); // the composer wraps long lines now
    }
}
