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
use super::md;
use unicode_width::UnicodeWidthChar;

/// Semantic colors. Named ANSI colors only, so Terminal.app (256 colors) and light/dark profiles work.
pub struct Theme {
    pub user: Style,
    pub text: Style,
    pub dim: Style,
    pub error: Style,
    pub accent: Style,
    pub bar: Style,
    pub code: Style,
    pub heading: Style,
    pub quote: Style,
    pub link: Style,
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
            code: Style::new().fg(Color::Yellow),
            heading: Style::new().fg(Color::Cyan),
            quote: Style::new().add_modifier(Modifier::DIM),
            link: Style::new().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
        }
    }
}

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
    fn lines(&self, width: usize, t: &Theme) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut out = vec![];
        let mut starts = vec![];
        for f in &self.files {
            starts.push(out.len());
            out.push(Line::styled(format!("=== {}  +{} -{}", f.path, f.adds, f.dels), t.accent.add_modifier(Modifier::BOLD)));
            if f.binary {
                out.push(Line::styled("binary file changed", t.dim));
            }
            for raw in f.patch.lines() {
                out.extend(diff_line(raw, width, t));
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
            AppEvent::Resize | AppEvent::Delivered => {}
            AppEvent::Tick => self.tick += 1,
            AppEvent::Engine { depth, agent, ev } => self.engine(depth, agent, ev),
            AppEvent::Ask { action, resources, save, reply } => {
                self.dialogs.push_back(Permission { action, resources, save, reply, feedback: None });
                self.scroll = 0;
            }
            AppEvent::Background(line) => self.push_info(format!("[{line}]")),
            AppEvent::Questions { questions, reply } => {
                if questions.is_empty() {
                    let _ = reply.send(Some(vec![]));
                } else {
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
            KeyCode::Char('p') if ctrl => return Action::Palette,
            KeyCode::Char('o') if ctrl => {
                self.level = (self.level + 1) % 3;
                self.hint = Some(["detail: compact (no reasoning, no diffs)", "detail: normal (diffs, reasoning summarized)", "detail: full (reasoning, tool output, every subagent call)"][self.level as usize].into());
            }
            KeyCode::Char('q') if ctrl => {
                if self.input.trim().is_empty() {
                    return Action::None;
                }
                return if self.running && !self.is_command() { self.enqueue(true) } else { self.submit() };
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
        let (lines, starts) = v.lines(width, &self.theme);
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
        let mut out: Vec<Line<'static>> = vec![];
        let row = &self.rows[first];
        match row {
            Row::User(s) => {
                if first > 0 {
                    out.push(Line::default());
                }
                for (n, l) in wrap(s, width - 2).into_iter().enumerate() {
                    out.push(Line::from(vec![Span::styled(if n == 0 { "> " } else { "  " }, t.user), Span::styled(l, t.text)]));
                }
            }
            Row::Assistant(s) => out.extend(md::render(s, width, t)),
            Row::Reasoning(s) => match self.level {
                0 => {}
                1 => {
                    let lines = s.lines().filter(|l| !l.trim().is_empty()).count();
                    let head = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    let text = format!("thinking ({lines} line{}): {}", if lines == 1 { "" } else { "s" }, head);
                    out.push(Line::styled(clip_width(&text, width.saturating_sub(1)), t.dim.add_modifier(Modifier::ITALIC)));
                }
                _ => out.extend(wrap(s.trim(), width).into_iter().map(|l| Line::styled(l, t.dim.add_modifier(Modifier::ITALIC)))),
            },
            Row::Tool { depth, agent, .. } if *depth > 0 && self.level < 2 => {
                // A run of a subagent's calls becomes one line (Ctrl-O shows them all).
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
                let text = format!("{pad}[{agent}] {n} tool call{}{}: {} > {last_name} {last_summary}", if n == 1 { "" } else { "s" }, if failed > 0 { format!(", {failed} failed") } else { String::new() }, if running { "now" } else { "last" });
                out.extend(wrap(&text, width).into_iter().map(|l| Line::styled(l, if failed > 0 { t.error } else { t.dim })));
            }
            Row::Tool { depth, agent, name, summary, state, detail, output } => {
                let pad = "  ".repeat(*depth);
                let who = if *depth > 0 { format!("[{agent}] ") } else { String::new() };
                out.extend(wrap(&format!("{pad}{who}> {name} {summary}"), width).into_iter().map(|l| Line::styled(l, t.dim)));
                if let (Some(d), true) = (detail, self.level >= 1) {
                    let cap = if self.level >= 2 { usize::MAX } else { DETAIL_LINES };
                    let lines: Vec<&str> = d.lines().collect();
                    for raw in lines.iter().take(cap) {
                        out.extend(diff_line(raw, width.saturating_sub(pad.len() + 2), t).into_iter().map(|mut l| {
                            l.spans.insert(0, Span::raw(format!("{pad}  ")));
                            l
                        }));
                    }
                    if lines.len() > cap {
                        out.push(Line::styled(format!("{pad}  ... {} more lines (Ctrl-O shows all)", lines.len() - cap), t.dim));
                    }
                }
                if let ToolState::Failed(msg) = state {
                    out.extend(wrap(&format!("{pad}  failed: {}", msg.lines().next().unwrap_or("")), width).into_iter().map(|l| Line::styled(l, t.error)));
                } else if self.level >= 2 && !output.is_empty() && detail.is_none() {
                    out.extend(output.lines().take(12).flat_map(|l| wrap(&format!("{pad}  {l}"), width)).map(|l| Line::styled(l, t.dim)));
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
                    out.extend(wrap(&format!("  {mark} {}", td.content), width).into_iter().map(|l| Line::styled(l, t.dim)));
                }
            }
            Row::Shell(s) => out.extend(wrap(s.trim_end(), width).into_iter().map(|l| Line::styled(l, t.text))),
            Row::Info(s) | Row::Usage(s) => out.extend(wrap(s, width).into_iter().map(|l| Line::styled(l, t.dim))),
            Row::Error(s) => out.extend(wrap(s, width).into_iter().map(|l| Line::styled(l, t.error))),
        }
        out
    }

    /// The dialog, form or picker that replaces the composer: (title, lines, cursor inside it).
    fn modal_lines(&self, width: usize, max_h: usize) -> Option<(String, Vec<Line<'static>>, Option<(u16, u16)>)> {
        let t = &self.theme;
        if let Some(d) = self.dialogs.front() {
            return Some((" permission ".into(), dialog_lines(d, width), None));
        }
        if let Some(f) = self.forms.front() {
            let q = f.q();
            let mut v = vec![];
            let head = if q.header.is_empty() { q.question.clone() } else { format!("[{}] {}", q.header, q.question) };
            for l in wrap(&head, width.saturating_sub(2)) {
                v.push(Line::styled(l, Style::new().add_modifier(Modifier::BOLD)));
            }
            for (i, o) in q.options.iter().enumerate() {
                let mark = if q.multiple { if f.checked[i] { "[x] " } else { "[ ] " } } else { "" };
                let text = clip_width(&format!("{} {}. {mark}{}{}", if f.cur == i { ">" } else { " " }, i + 1, o.label, if o.description.is_empty() { String::new() } else { format!(" - {}", o.description) }), width.saturating_sub(2));
                v.push(if f.cur == i { Line::styled(text, t.bar) } else { Line::raw(text) });
            }
            let own = format!("{} Type your own answer", if f.cur == q.options.len() { ">" } else { " " });
            v.push(if f.cur == q.options.len() { Line::styled(own, t.bar) } else { Line::raw(own) });
            let mut cursor = None;
            match &f.typing {
                Some(text) => {
                    let line = format!("answer: {text}");
                    cursor = Some((display_width(&line) as u16, v.len() as u16));
                    v.push(Line::raw(line));
                }
                None => v.push(Line::styled(if q.multiple { "Space toggles, Enter confirms, 1-9 toggles, Esc dismisses" } else { "Enter or 1-9 selects, Esc dismisses" }, t.dim)),
            }
            let title = format!(" question {}/{} ", f.qi + 1, f.questions.len());
            return Some((title, v, cursor));
        }
        if let Some(p) = &self.picker {
            let vis = p.visible();
            let rows = max_h.saturating_sub(2).clamp(1, 12);
            let first = (p.sel + 1).saturating_sub(rows);
            let mut v = vec![Line::raw(format!("filter: {}", p.filter))];
            let cursor = Some((display_width(&format!("filter: {}", p.filter)) as u16, 0));
            if vis.is_empty() {
                v.push(Line::styled("no match", t.dim));
            }
            for (n, &i) in vis.iter().enumerate().skip(first).take(rows) {
                let it = &p.items[i];
                let text = clip_width(&format!(" {}  {}", it.label, it.detail), width.saturating_sub(2));
                v.push(if n == p.sel { Line::styled(text, t.bar) } else { Line::raw(text) });
            }
            let title = format!(" {} ({}) - Enter picks, Esc closes ", p.title, vis.len());
            return Some((title, v, cursor));
        }
        None
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
        if self.jobs > 0 {
            parts.push(format!("{} job{} (/jobs)", self.jobs, if self.jobs == 1 { "" } else { "s" }));
        }
        let mut s = format!(" {}", parts.join(" | "));
        let right = if let Some(h) = &self.hint {
            h.clone()
        } else if let (true, Some((at, attempt, _))) = (self.running, &self.retry) {
            let left = at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
            format!("{spin} retrying in {left}s (attempt {attempt}), Esc stops")
        } else if self.running {
            format!("{spin} running: Enter steers, Ctrl-Q queues, Esc stops")
        } else if self.scroll > 0 {
            "scrolled (Ctrl-End follows)".to_string()
        } else {
            "Enter sends, Ctrl-P commands, Ctrl-D quits".to_string()
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

/// One line of a unified diff, colored by its first character.
fn diff_line(raw: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let style = match raw.chars().next() {
        Some('+') if !raw.starts_with("+++") => Style::new().fg(Color::Green),
        Some('-') if !raw.starts_with("---") => t.error,
        Some('@') | Some('*') => t.accent,
        _ => t.dim,
    };
    wrap(raw, width.max(8)).into_iter().map(|l| Line::styled(l, style)).collect()
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

fn render_viewer(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let theme_bar = app.theme.bar;
    let dim = app.theme.dim;
    let h = parts[1].height as usize;
    app.view_h = h;
    app.comp_w = (area.width as usize).saturating_sub(2);
    let Some(v) = app.viewer.as_mut() else { return };
    let (lines, starts) = v.lines(area.width as usize, &app.theme);
    v.scroll = v.scroll.min(lines.len().saturating_sub(h));
    let (adds, dels): (usize, usize) = v.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
    let at_end = v.scroll + h >= lines.len();
    let cur = if at_end { v.files.len() } else { starts.iter().rposition(|&s| s <= v.scroll).map_or(0, |i| i + 1) };
    f.render_widget(Paragraph::new(format!(" changes since the session started: {} file(s) +{adds} -{dels}", v.files.len())).style(theme_bar), parts[0]);
    f.render_widget(Paragraph::new(lines.iter().skip(v.scroll).take(h).cloned().collect::<Vec<_>>()), parts[1]);
    let at = if lines.len() <= h { "all".to_string() } else { format!("{}%", (v.scroll + h).min(lines.len()) * 100 / lines.len()) };
    f.render_widget(Paragraph::new(format!(" j/k Space/b scroll  n/p file  g/G ends  q closes   file {cur}/{}  {at}", v.files.len())).style(dim), parts[2]);
}

pub fn render(f: &mut Frame, app: &mut App) {
    if app.viewer.is_some() && app.dialogs.is_empty() && app.forms.is_empty() {
        return render_viewer(f, app);
    }
    let area = f.area();
    let max_modal = (area.height as usize).saturating_sub(5).max(3);
    let modal = app.modal_lines(area.width as usize, max_modal);
    let comp_w = (area.width as usize).saturating_sub(2).max(1);
    app.comp_w = comp_w;
    let comp_lines = app.layout(comp_w);
    let visible = comp_lines.len().min(MAX_COMPOSER_LINES);
    let bottom_h = modal.as_ref().map_or(visible as u16 + 2, |m| m.1.len() as u16 + 2).min(area.height.saturating_sub(2).max(1));
    // Prompts waiting for delivery sit between the transcript and the composer.
    let shown_q = app.queued.len().min(3);
    let strip_h = (shown_q + (app.queued.len() > 3) as usize).min((area.height as usize).saturating_sub(6)) as u16;
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(strip_h), Constraint::Length(bottom_h), Constraint::Length(1)]).split(area);
    let (main, strip, bottom, status) = (parts[0], parts[1], parts[2], parts[3]);
    if strip_h > 0 {
        let mut ql: Vec<Line> = app
            .queued
            .iter()
            .take(shown_q)
            .map(|q| Line::styled(clip_width(&format!(" {} {}", if q.steer { "next step>" } else { "after turn>" }, q.text.lines().next().unwrap_or("")), area.width as usize), app.theme.dim))
            .collect();
        if app.queued.len() > 3 {
            ql.push(Line::styled(format!(" +{} more (/queue)", app.queued.len() - 3), app.theme.dim));
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

    if let Some((title, lines, cursor)) = modal {
        let block = Block::bordered().title(title).border_style(app.theme.accent);
        let inner = block.inner(bottom);
        f.render_widget(Paragraph::new(lines).block(block), bottom);
        if let Some((col, row)) = cursor {
            f.set_cursor_position((inner.x + col.min(inner.width.saturating_sub(1)), inner.y + row));
        }
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
        assert!(joined.contains("> hi") && joined.contains("Hello") && joined.contains("> read a.rs") && joined.contains("failed: no such file"), "{joined}");
        // Subagent prose is hidden, its tools are indented.
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Text("secret".into()) });
        a.update(AppEvent::Engine { depth: 1, agent: "explore".into(), ev: Ev::Tool { name: "grep".into(), summary: "x".into(), detail: None } });
        let joined = screen(&mut a, 40, 14).join("\n");
        assert!(!joined.contains("secret") && joined.contains("  [explore] 1 tool call: now > grep x"), "{joined}");
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
        assert!(joined.contains("question 1/3") && joined.contains("[H] Which?") && joined.contains("1. red") && joined.contains("Type your own answer"), "{joined}");
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
        assert!(joined.contains("model (3)") && joined.contains("anthropic/sonnet") && joined.contains("about openai/gpt"), "{joined}");
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
        assert!(s.contains("> edit x.rs") && s.contains("  -old line") && s.contains("  +new line") && s.contains("+added 23") && !s.contains("+added 24") && s.contains("... 16 more lines (Ctrl-O shows all)"), "{s}");
        let lines = a.lines_window(60, usize::MAX).0;
        let color = |needle: &str| lines.iter().find(|l| l.spans.iter().any(|s| s.content.contains(needle))).map(|l| l.style.fg);
        assert_eq!((color("-old line"), color("+new line")), (Some(Some(Color::Red)), Some(Some(Color::Green))));
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
        assert!(s.contains("> edit x.rs") && !s.contains("-old line") && !s.contains("+added"), "level 0 shows only the call lines: {s}");
    }

    #[test]
    fn reasoning_is_summarized_hidden_or_shown() {
        let mut a = App::new("m", "build");
        text(&mut a, Ev::Reasoning("Let me think\nabout the *edge* case".into()));
        text(&mut a, Ev::Reasoning(" more".into()));
        text(&mut a, Ev::Text("Answer".into()));
        assert_eq!(a.rows.len(), 2); // reasoning chunks join into one row
        let s = screen(&mut a, 70, 10).join("\n");
        assert!(s.contains("thinking (2 lines): Let me think about the *edge* case more") && s.contains("Answer"), "{s}");
        a.level = 2;
        let s = screen(&mut a, 40, 10).join("\n");
        assert!(s.contains("Let me think\nabout the *edge* case more") && !s.contains("thinking ("), "{s}");
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
        assert!(s[0].contains("2 file(s) +61 -1") && s[1].contains("=== a.rs  +60 -0") && s[2].contains("+line 0") && s[11].contains("file 1/2"), "{s:?}");
        press(&mut a, KeyCode::Char('j'));
        assert!(screen(&mut a, 60, 12)[1].contains("+line 0"), "scrolled one line");
        press(&mut a, KeyCode::Char('n')); // next file
        let s = screen(&mut a, 60, 12);
        assert!(s.iter().any(|l| l.contains("=== b.rs  +1 -1")) && s[11].contains("file 2/2"), "{s:?}"); // a short last file cannot reach the top
        press(&mut a, KeyCode::Char('p'));
        assert!(screen(&mut a, 60, 12).iter().any(|l| l.contains("+line 5")), "p goes back to the first file's lines");
        press(&mut a, KeyCode::Char('G'));
        assert!(screen(&mut a, 60, 12).iter().any(|l| l.contains("+here")));
        press(&mut a, KeyCode::Char('g'));
        assert!(screen(&mut a, 60, 12)[1].contains("=== a.rs"));
        // Typing goes to the viewer, not the composer; q closes it.
        press(&mut a, KeyCode::Char('x'));
        assert!(a.input.is_empty());
        press(&mut a, KeyCode::Char('q'));
        assert!(a.viewer.is_none());
        // An engine dialog is shown above the viewer.
        a.viewer = Some(Viewer { files: vec![file("a.rs", "+x")], scroll: 0 });
        let (tx, _rx) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec!["ls".into()], save: vec![], reply: tx });
        assert!(screen(&mut a, 60, 12).join("\n").contains("Permission needed: shell"));
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

    #[test]
    fn the_cursor_stays_visible_in_a_long_line() {
        let mut a = App::new("m", "build");
        type_str(&mut a, &"x".repeat(50));
        type_str(&mut a, "END");
        let s = screen(&mut a, 20, 8);
        assert!(s[s.len() - 3].contains("xxxEND"), "{s:?}");
    }
}
