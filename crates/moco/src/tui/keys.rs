//! Rebindable keys. Typing, Enter, Tab, arrows and the dialogs' own keys are fixed; the actions below can be
//! moved with `tui.keys` in the config, and `tui.leader` defines what the leader key (Ctrl-X) plus a key does.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Close the completion popup, stop the running turn, or jump back to the newest output.
    Interrupt,
    /// Leave (only with an empty composer and no turn running; otherwise the key edits as usual).
    Quit,
    Palette,
    Newline,
    /// Send as "after the turn" while a turn runs.
    Queue,
    /// Cycle the detail level.
    Detail,
    /// Step through the model's reasoning levels (and back to the default).
    Effort,
    Sidebar,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Leader,
}

const NAMES: &[(&str, KeyAction)] = &[
    ("interrupt", KeyAction::Interrupt),
    ("quit", KeyAction::Quit),
    ("palette", KeyAction::Palette),
    ("newline", KeyAction::Newline),
    ("queue", KeyAction::Queue),
    ("detail", KeyAction::Detail),
    ("effort", KeyAction::Effort),
    ("sidebar", KeyAction::Sidebar),
    ("page_up", KeyAction::PageUp),
    ("page_down", KeyAction::PageDown),
    ("top", KeyAction::Top),
    ("bottom", KeyAction::Bottom),
    ("leader", KeyAction::Leader),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeySpec {
    pub fn matches(&self, k: &KeyEvent) -> bool {
        let mut got = k.modifiers;
        let mut want = self.mods;
        if let (KeyCode::Char(a), KeyCode::Char(b)) = (k.code, self.code) {
            // Shift is already in the character; Ctrl and Alt letters compare without case.
            got.remove(KeyModifiers::SHIFT);
            want.remove(KeyModifiers::SHIFT);
            return got == want && (a == b || (!got.is_empty() && a.eq_ignore_ascii_case(&b)));
        }
        k.code == self.code && got == want
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("Ctrl-");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("Alt-");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("Shift-");
        }
        s.push_str(&match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::F(n) => format!("F{n}"),
            other => format!("{other:?}"),
        });
        s
    }
}

/// `"ctrl+p"`, `"alt+enter"`, `"pageup"`, `"f2"`, `"x"`.
pub fn parse_key(spec: &str) -> Result<KeySpec, String> {
    let lower = spec.trim().to_lowercase();
    if lower.is_empty() {
        return Err("empty key".into());
    }
    let mut mods = KeyModifiers::NONE;
    let mut parts: Vec<&str> = lower.split('+').collect();
    if lower == "+" || lower.ends_with("++") {
        // "ctrl++" means Ctrl and the plus key.
        parts.retain(|p| !p.is_empty());
        parts.push("+");
    }
    let key = parts.pop().ok_or("empty key")?;
    for m in parts {
        match m.trim() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier '{other}' in '{spec}'")),
        }
    }
    let code = match key {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => KeyCode::F(f[1..].parse().unwrap()),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next().unwrap()),
        other => return Err(format!("unknown key '{other}' in '{spec}'")),
    };
    Ok(KeySpec { code, mods })
}

pub struct Keymap {
    binds: Vec<(KeyAction, Vec<KeySpec>)>,
    /// Key after the leader, and what it does: a `/command` or an action name (`sidebar`, `detail`, `palette`, `quit`).
    pub leader: Vec<(KeySpec, String)>,
}

fn defaults() -> Vec<(KeyAction, Vec<&'static str>)> {
    vec![
        (KeyAction::Interrupt, vec!["esc"]),
        (KeyAction::Quit, vec!["ctrl+d"]),
        (KeyAction::Palette, vec!["ctrl+p"]),
        (KeyAction::Newline, vec!["alt+enter", "ctrl+j"]),
        (KeyAction::Queue, vec!["ctrl+q"]),
        (KeyAction::Detail, vec!["ctrl+o"]),
        (KeyAction::Effort, vec!["ctrl+t"]),
        (KeyAction::Sidebar, vec![]),
        (KeyAction::PageUp, vec!["pageup"]),
        (KeyAction::PageDown, vec!["pagedown"]),
        (KeyAction::Top, vec!["ctrl+home"]),
        (KeyAction::Bottom, vec!["ctrl+end"]),
        (KeyAction::Leader, vec!["ctrl+x"]),
    ]
}

const DEFAULT_LEADER: &[(&str, &str)] = &[
    ("n", "/new"),
    ("l", "/sessions"),
    ("m", "/model"),
    ("a", "/agent"),
    ("d", "/diff"),
    ("c", "/compact"),
    ("u", "/undo"),
    ("r", "/redo"),
    ("j", "/jobs"),
    ("t", "/theme"),
    ("h", "/help"),
    ("q", "/exit"),
    ("b", "sidebar"),
    ("o", "detail"),
    ("e", "/effort"),
    ("p", "palette"),
];

impl Default for Keymap {
    fn default() -> Self {
        Keymap::from_config(&BTreeMap::new(), &BTreeMap::new()).0
    }
}

impl Keymap {
    /// Defaults with the config's changes applied. Mistakes are returned as warnings and the default stays.
    pub fn from_config(keys: &BTreeMap<String, Value>, leader: &BTreeMap<String, String>) -> (Keymap, Vec<String>) {
        let mut warnings = vec![];
        for name in keys.keys() {
            if !NAMES.iter().any(|(n, _)| n == name) {
                warnings.push(format!("tui.keys: unknown action '{name}' (actions: {})", NAMES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")));
            }
        }
        let mut binds: Vec<(KeyAction, Vec<KeySpec>)> = vec![];
        for (action, default) in defaults() {
            let name = NAMES.iter().find(|(_, a)| *a == action).map(|(n, _)| *n).unwrap();
            let strings: Vec<String> = match keys.get(name) {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                Some(Value::Null) => vec![],
                Some(_) => {
                    warnings.push(format!("tui.keys.{name}: expected a key or a list of keys"));
                    default.iter().map(|s| s.to_string()).collect()
                }
                None => default.iter().map(|s| s.to_string()).collect(),
            };
            let mut specs = vec![];
            for s in &strings {
                match parse_key(s) {
                    Ok(k) if k.code == KeyCode::Char('c') && k.mods == KeyModifiers::CONTROL => warnings.push(format!("tui.keys.{name}: Ctrl-C cannot be rebound (it always interrupts, clears or quits)")),
                    Ok(k) => specs.push(k),
                    Err(e) => warnings.push(format!("tui.keys.{name}: {e}")),
                }
            }
            if specs.is_empty() && !strings.is_empty() {
                specs = default.iter().filter_map(|s| parse_key(s).ok()).collect(); // everything given was invalid
            }
            binds.push((action, specs));
        }
        // The same key on two actions: the earlier action wins.
        for i in 0..binds.len() {
            for j in 0..i {
                let (a, b) = (binds[j].1.clone(), binds[i].0);
                let before = binds[i].1.len();
                binds[i].1.retain(|k| !a.contains(k));
                if binds[i].1.len() < before {
                    warnings.push(format!("tui.keys: a key is bound to both '{}' and '{}'; '{}' keeps it", name_of(binds[j].0), name_of(b), name_of(binds[j].0)));
                }
            }
        }
        let mut table: Vec<(KeySpec, String)> = vec![];
        let given = !leader.is_empty();
        let pairs: Vec<(String, String)> = if given { leader.iter().map(|(k, v)| (k.clone(), v.clone())).collect() } else { DEFAULT_LEADER.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        // Config entries are added to the defaults rather than replacing them; an empty string removes one.
        let mut all: BTreeMap<String, String> = DEFAULT_LEADER.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (k, v) in pairs {
            all.insert(k, v);
        }
        for (k, v) in all {
            if v.is_empty() {
                continue;
            }
            match parse_key(&k) {
                Ok(spec) => {
                    if !v.starts_with('/') && !["sidebar", "detail", "palette", "quit"].contains(&v.as_str()) {
                        warnings.push(format!("tui.leader.{k}: '{v}' is neither a /command nor one of sidebar, detail, palette, quit"));
                    } else {
                        table.push((spec, v));
                    }
                }
                Err(e) => warnings.push(format!("tui.leader: {e}")),
            }
        }
        (Keymap { binds, leader: table }, warnings)
    }

    pub fn action(&self, k: &KeyEvent) -> Option<KeyAction> {
        self.binds.iter().find(|(_, keys)| keys.iter().any(|s| s.matches(k))).map(|(a, _)| *a)
    }

    pub fn label(&self, action: KeyAction) -> String {
        self.binds.iter().find(|(a, _)| *a == action).and_then(|(_, k)| k.first()).map_or("(unbound)".to_string(), KeySpec::label)
    }

    pub fn leader_target(&self, k: &KeyEvent) -> Option<&str> {
        self.leader.iter().find(|(s, _)| s.matches(k)).map(|(_, v)| v.as_str())
    }

    /// "n /new  l /sessions ..." for the hint shown after the leader key.
    pub fn leader_hint(&self) -> String {
        self.leader.iter().map(|(k, v)| format!("{} {}", k.label().to_lowercase(), v.trim_start_matches('/'))).collect::<Vec<_>>().join("  ")
    }
}

fn name_of(a: KeyAction) -> &'static str {
    NAMES.iter().find(|(_, x)| *x == a).map(|(n, _)| *n).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use serde_json::json;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent { code, modifiers: mods, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    #[test]
    fn key_names_parse() {
        assert_eq!(parse_key("ctrl+p").unwrap(), KeySpec { code: KeyCode::Char('p'), mods: KeyModifiers::CONTROL });
        assert_eq!(parse_key("Alt+Enter").unwrap(), KeySpec { code: KeyCode::Enter, mods: KeyModifiers::ALT });
        assert_eq!(parse_key("ctrl+shift+pgup").unwrap().mods, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(parse_key("f2").unwrap().code, KeyCode::F(2));
        assert_eq!(parse_key("ctrl++").unwrap(), KeySpec { code: KeyCode::Char('+'), mods: KeyModifiers::CONTROL });
        assert_eq!(parse_key("space").unwrap().code, KeyCode::Char(' '));
        for bad in ["", "ctrl+", "hyper+x", "ctrl+nope", "f13"] {
            assert!(parse_key(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_key("ctrl+alt+x").unwrap().label(), "Ctrl-Alt-X");
        assert_eq!(parse_key("pagedown").unwrap().label(), "PageDown");
    }

    #[test]
    fn matching_ignores_shift_on_characters_and_case_of_control_letters() {
        let ctrl_p = parse_key("ctrl+p").unwrap();
        assert!(ctrl_p.matches(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL)));
        assert!(ctrl_p.matches(&ev(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)));
        assert!(!ctrl_p.matches(&ev(KeyCode::Char('p'), KeyModifiers::NONE)));
        assert!(!ctrl_p.matches(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
        let plain_x = parse_key("x").unwrap();
        assert!(plain_x.matches(&ev(KeyCode::Char('x'), KeyModifiers::NONE)) && !plain_x.matches(&ev(KeyCode::Char('X'), KeyModifiers::SHIFT)));
        assert!(parse_key("alt+enter").unwrap().matches(&ev(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(!parse_key("enter").unwrap().matches(&ev(KeyCode::Enter, KeyModifiers::ALT)));
    }

    #[test]
    fn defaults_and_overrides() {
        let km = Keymap::default();
        assert_eq!(km.action(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL)), Some(KeyAction::Palette));
        assert_eq!(km.action(&ev(KeyCode::Char('j'), KeyModifiers::CONTROL)), Some(KeyAction::Newline));
        assert_eq!(km.action(&ev(KeyCode::Enter, KeyModifiers::ALT)), Some(KeyAction::Newline));
        assert_eq!(km.action(&ev(KeyCode::Char('a'), KeyModifiers::NONE)), None);
        assert_eq!(km.label(KeyAction::Palette), "Ctrl-P");
        assert_eq!(km.label(KeyAction::Sidebar), "(unbound)");
        assert_eq!(km.leader_target(&ev(KeyCode::Char('n'), KeyModifiers::NONE)), Some("/new"));

        let keys = json!({"palette": "ctrl+k", "quit": ["ctrl+d", "ctrl+q"], "sidebar": "f3", "page_up": null}).as_object().unwrap().clone().into_iter().collect();
        let leader: BTreeMap<String, String> = [("n".to_string(), "/new my-agent".to_string()), ("b".to_string(), String::new()), ("z".to_string(), "detail".to_string())].into();
        let (km, warnings) = Keymap::from_config(&keys, &leader);
        // ctrl+q was given to quit and is also the default of queue: quit comes first and keeps it.
        assert_eq!(km.action(&ev(KeyCode::Char('k'), KeyModifiers::CONTROL)), Some(KeyAction::Palette));
        assert_eq!(km.action(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL)), None);
        assert_eq!(km.action(&ev(KeyCode::Char('q'), KeyModifiers::CONTROL)), Some(KeyAction::Quit));
        assert_eq!(km.action(&ev(KeyCode::F(3), KeyModifiers::NONE)), Some(KeyAction::Sidebar));
        assert_eq!(km.action(&ev(KeyCode::PageUp, KeyModifiers::NONE)), None, "null unbinds");
        assert!(warnings.iter().any(|w| w.contains("'quit' and 'queue'")), "{warnings:?}");
        assert_eq!(km.leader_target(&ev(KeyCode::Char('n'), KeyModifiers::NONE)), Some("/new my-agent"));
        assert_eq!(km.leader_target(&ev(KeyCode::Char('b'), KeyModifiers::NONE)), None, "an empty string removes a leader key");
        assert_eq!(km.leader_target(&ev(KeyCode::Char('z'), KeyModifiers::NONE)), Some("detail"));
        assert_eq!(km.leader_target(&ev(KeyCode::Char('l'), KeyModifiers::NONE)), Some("/sessions"), "unmentioned defaults stay");
    }

    #[test]
    fn mistakes_become_warnings_and_keep_the_defaults() {
        let keys = json!({"palete": "ctrl+k", "detail": "ctrl+nope", "interrupt": "ctrl+c", "newline": 5}).as_object().unwrap().clone().into_iter().collect();
        let leader: BTreeMap<String, String> = [("x".to_string(), "frobnicate".to_string())].into();
        let (km, w) = Keymap::from_config(&keys, &leader);
        let all = w.join("\n");
        assert!(all.contains("unknown action 'palete'") && all.contains("tui.keys.detail: unknown key 'nope'") && all.contains("Ctrl-C cannot be rebound") && all.contains("tui.keys.newline: expected") && all.contains("tui.leader.x: 'frobnicate'"), "{all}");
        assert_eq!(km.action(&ev(KeyCode::Char('o'), KeyModifiers::CONTROL)), Some(KeyAction::Detail));
        assert_eq!(km.action(&ev(KeyCode::Esc, KeyModifiers::NONE)), Some(KeyAction::Interrupt));
        assert_eq!(km.action(&ev(KeyCode::Char('j'), KeyModifiers::CONTROL)), Some(KeyAction::Newline));
        assert_eq!(km.action(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL)), None);
    }
}
