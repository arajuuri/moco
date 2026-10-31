//! Colors and styles by semantic role, built-in themes and theme files.
//!
//! A theme file is JSON, `<app dir>/config/themes/<name>.json` or `<project>/.moco/themes/<name>.json`:
//! `{"extends": "dark", "user": "cyan bold", "code": "#ffaf00", "bar": "white on 238"}`. Each role is a
//! space-separated style: a color (`red`, `lightblue`, `gray`, `#rrggbb`, a 0-255 index, `default`),
//! optionally `on <color>` for the background, and modifiers (`bold dim italic underline reverse`).
//! Terminals without truecolor (Terminal.app) get the nearest of the 256 colors for `#rrggbb`.
use ratatui::prelude::*;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Theme {
    pub user: Style,
    pub text: Style,
    pub dim: Style,
    pub error: Style,
    pub accent: Style,
    /// The status bar, and the selected row of lists.
    pub bar: Style,
    pub code: Style,
    pub heading: Style,
    pub quote: Style,
    pub link: Style,
    pub added: Style,
    pub removed: Style,
}

pub const BUILTIN: &[&str] = &["dark", "light", "mono"];

fn st() -> Style {
    Style::new()
}

impl Theme {
    /// For dark backgrounds. Only named ANSI colors, so the terminal's own palette decides the shades.
    pub fn dark() -> Theme {
        Theme {
            user: st().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            text: st(),
            dim: st().add_modifier(Modifier::DIM),
            error: st().fg(Color::Red),
            accent: st().fg(Color::Cyan),
            bar: st().add_modifier(Modifier::REVERSED),
            code: st().fg(Color::Yellow),
            heading: st().fg(Color::Cyan),
            quote: st().add_modifier(Modifier::DIM),
            link: st().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
            added: st().fg(Color::Green),
            removed: st().fg(Color::Red),
        }
    }

    /// For light backgrounds: cyan and yellow are unreadable on white, so blue, magenta and dark gray.
    pub fn light() -> Theme {
        Theme {
            user: st().fg(Color::Blue).add_modifier(Modifier::BOLD),
            text: st(),
            dim: st().fg(Color::DarkGray),
            error: st().fg(Color::Red),
            accent: st().fg(Color::Blue),
            bar: st().add_modifier(Modifier::REVERSED),
            code: st().fg(Color::Magenta),
            heading: st().fg(Color::Blue),
            quote: st().fg(Color::DarkGray),
            link: st().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
            added: st().fg(Color::Green),
            removed: st().fg(Color::Red),
        }
    }

    /// No colors at all, only bold, dim, underline and reverse (monochrome terminals, screenshots, taste).
    pub fn mono() -> Theme {
        Theme {
            user: st().add_modifier(Modifier::BOLD),
            text: st(),
            dim: st().add_modifier(Modifier::DIM),
            error: st().add_modifier(Modifier::BOLD),
            accent: st(),
            bar: st().add_modifier(Modifier::REVERSED),
            code: st().add_modifier(Modifier::ITALIC),
            heading: st().add_modifier(Modifier::BOLD),
            quote: st().add_modifier(Modifier::DIM),
            link: st().add_modifier(Modifier::UNDERLINED),
            added: st().add_modifier(Modifier::BOLD),
            removed: st().add_modifier(Modifier::DIM),
        }
    }

    pub fn builtin(name: &str) -> Option<Theme> {
        match name {
            "dark" => Some(Theme::dark()),
            "light" => Some(Theme::light()),
            "mono" => Some(Theme::mono()),
            _ => None,
        }
    }

    fn role(&mut self, name: &str) -> Option<&mut Style> {
        Some(match name {
            "user" => &mut self.user,
            "text" => &mut self.text,
            "dim" => &mut self.dim,
            "error" => &mut self.error,
            "accent" => &mut self.accent,
            "bar" => &mut self.bar,
            "code" => &mut self.code,
            "heading" => &mut self.heading,
            "quote" => &mut self.quote,
            "link" => &mut self.link,
            "added" => &mut self.added,
            "removed" => &mut self.removed,
            _ => return None,
        })
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

/// Does the terminal take `#rrggbb` colors as they are?
pub fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"))
}

/// "auto": light when the terminal says its background is (COLORFGBG, set by iTerm2, rxvt and others), dark otherwise.
pub fn auto_name() -> &'static str {
    match std::env::var("COLORFGBG").ok().and_then(|v| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok())) {
        Some(7) | Some(15) => "light",
        _ => "dark",
    }
}

/// Index in the xterm 256-color palette nearest to an RGB color.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| -> (u8, u8) {
        // (cube index 0..5, its channel value)
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let i = (0..6).min_by_key(|&i| (STEPS[i] as i32 - v as i32).abs()).unwrap();
        (i as u8, STEPS[i])
    };
    let (ri, rv) = level(r);
    let (gi, gv) = level(g);
    let (bi, bv) = level(b);
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let dist = |a: (u8, u8, u8)| (a.0 as i32 - r as i32).pow(2) + (a.1 as i32 - g as i32).pow(2) + (a.2 as i32 - b as i32).pow(2);
    let cube_d = dist((rv, gv, bv));
    // The 24-step gray ramp 232..=255 (8, 18, ..., 238).
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi2 = ((avg - 8 + 5) / 10).clamp(0, 23) as u8;
    let gv2 = 8 + 10 * gi2;
    if dist((gv2, gv2, gv2)) < cube_d { 232 + gi2 } else { cube }
}

fn color(tok: &str, truecolor: bool) -> Option<Color> {
    let t = tok.to_lowercase();
    if let Some(hex) = t.strip_prefix('#') {
        if hex.len() == 6 {
            let n = u32::from_str_radix(hex, 16).ok()?;
            let (r, g, b) = ((n >> 16) as u8, (n >> 8) as u8, n as u8);
            return Some(if truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(nearest_256(r, g, b)) });
        }
        return None;
    }
    if let Ok(n) = t.parse::<u8>() {
        return Some(Color::Indexed(n));
    }
    Some(match t.as_str() {
        "default" | "reset" | "none" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray, // ANSI "white" is the light gray
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" | "brightblack" => Color::DarkGray,
        "lightred" | "brightred" => Color::LightRed,
        "lightgreen" | "brightgreen" => Color::LightGreen,
        "lightyellow" | "brightyellow" => Color::LightYellow,
        "lightblue" | "brightblue" => Color::LightBlue,
        "lightmagenta" | "brightmagenta" => Color::LightMagenta,
        "lightcyan" | "brightcyan" => Color::LightCyan,
        "brightwhite" => Color::White,
        _ => return None,
    })
}

/// Parse `"red bold"`, `"#ffaf00 on 238 underline"`, ...
pub fn parse_style(spec: &str, truecolor: bool) -> Result<Style, String> {
    let mut style = Style::new();
    let mut toks = spec.split_whitespace();
    while let Some(tok) = toks.next() {
        match tok.to_lowercase().as_str() {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "dim" => style = style.add_modifier(Modifier::DIM),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" | "underlined" => style = style.add_modifier(Modifier::UNDERLINED),
            "reverse" | "reversed" => style = style.add_modifier(Modifier::REVERSED),
            "on" => {
                let c = toks.next().ok_or("\"on\" needs a color")?;
                style = style.bg(color(c, truecolor).ok_or_else(|| format!("unknown color '{c}'"))?);
            }
            other => style = style.fg(color(other, truecolor).ok_or_else(|| format!("unknown color or modifier '{tok}'"))?),
        }
    }
    Ok(style)
}

fn apply(base: &mut Theme, doc: &Value, truecolor: bool) -> Result<(), String> {
    let obj = doc.as_object().ok_or("a theme file must be a JSON object")?;
    for (k, v) in obj {
        if k == "extends" || k == "description" {
            continue;
        }
        let slot = base.role(k).ok_or_else(|| format!("unknown role '{k}' (roles: user text dim error accent bar code heading quote link added removed)"))?;
        *slot = parse_style(v.as_str().ok_or_else(|| format!("role '{k}' must be a string"))?, truecolor).map_err(|e| format!("role '{k}': {e}"))?;
    }
    Ok(())
}

fn theme_dirs(app_dir: &Path, cwd: &Path) -> Vec<std::path::PathBuf> {
    vec![app_dir.join("config/themes"), cwd.join(".moco/themes")]
}

/// Built-in names plus the theme files that can be found.
pub fn available(app_dir: &Path, cwd: &Path) -> Vec<String> {
    let mut v: Vec<String> = BUILTIN.iter().map(|s| s.to_string()).collect();
    for dir in theme_dirs(app_dir, cwd) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if let Some(n) = e.file_name().to_string_lossy().strip_suffix(".json") {
                if !v.iter().any(|x| x == n) {
                    v.push(n.to_string());
                }
            }
        }
    }
    v
}

/// A theme by name: `auto`, a built-in, or a file (project files win over the app directory's).
pub fn load(name: &str, app_dir: &Path, cwd: &Path, truecolor: bool) -> Result<Theme, String> {
    let name = if name == "auto" { auto_name() } else { name };
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("bad theme name '{name}'"));
    }
    let file = theme_dirs(app_dir, cwd).into_iter().rev().map(|d| d.join(format!("{name}.json"))).find(|p| p.is_file());
    match file {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let doc: Value = serde_json::from_str(&moco_core::config::strip_jsonc(&text)).map_err(|e| format!("{}: {e}", path.display()))?;
            let parent = doc["extends"].as_str().unwrap_or("dark");
            let mut theme = Theme::builtin(parent).ok_or_else(|| format!("{}: extends unknown built-in theme '{parent}'", path.display()))?;
            apply(&mut theme, &doc, truecolor).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(theme)
        }
        None => Theme::builtin(name).ok_or_else(|| format!("no theme '{name}' (built in: {}; or add {name}.json under {})", BUILTIN.join(", "), app_dir.join("config/themes").display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_parse_colors_modifiers_and_backgrounds() {
        let s = parse_style("red bold", true).unwrap();
        assert_eq!((s.fg, s.add_modifier.contains(Modifier::BOLD)), (Some(Color::Red), true));
        let s = parse_style("#ff8700 on 238 underline", true).unwrap();
        assert_eq!((s.fg, s.bg, s.add_modifier.contains(Modifier::UNDERLINED)), (Some(Color::Rgb(255, 135, 0)), Some(Color::Indexed(238)), true));
        // Without truecolor the same hex becomes the nearest palette entry.
        assert_eq!(parse_style("#ff8700", false).unwrap().fg, Some(Color::Indexed(208)));
        assert_eq!(parse_style("default", true).unwrap().fg, Some(Color::Reset));
        assert!(parse_style("reddish", true).unwrap_err().contains("reddish"));
        assert!(parse_style("on", true).is_err() && parse_style("#12345", true).is_err());
    }

    #[test]
    fn nearest_256_picks_cube_and_gray_ramp_entries() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(255, 0, 0), 196);
        assert_eq!(nearest_256(95, 135, 175), 67);
        assert_eq!(nearest_256(128, 128, 128), 244); // gray ramp: 8 + 10 * 12 = 128
        assert_eq!(nearest_256(8, 8, 8), 232);
    }

    #[test]
    fn theme_files_extend_a_builtin_and_report_mistakes() {
        let dir = std::env::temp_dir().join(format!("moco-theme-{}", std::process::id()));
        let (app, proj) = (dir.join("app"), dir.join("proj"));
        std::fs::create_dir_all(app.join("config/themes")).unwrap();
        std::fs::create_dir_all(proj.join(".moco/themes")).unwrap();
        std::fs::write(app.join("config/themes/sunset.json"), r##"{ // comment
            "extends": "light", "user": "#ff8700 bold", "code": "green" }"##).unwrap();
        std::fs::write(app.join("config/themes/broken.json"), r#"{"colour": "red"}"#).unwrap();
        std::fs::write(app.join("config/themes/badcolor.json"), r#"{"user": "nope"}"#).unwrap();
        std::fs::write(proj.join(".moco/themes/sunset.json"), r#"{"user": "magenta"}"#).unwrap();
        let t = load("sunset", &app, &dir.join("elsewhere"), true).unwrap();
        assert_eq!((t.user.fg, t.code.fg, t.dim.fg), (Some(Color::Rgb(255, 135, 0)), Some(Color::Green), Some(Color::DarkGray))); // light's dim
        assert_eq!(load("sunset", &app, &proj, true).unwrap().user.fg, Some(Color::Magenta)); // the project's file wins; extends defaults to dark
        assert!(load("broken", &app, &proj, true).unwrap_err().contains("unknown role 'colour'"));
        assert!(load("badcolor", &app, &proj, true).unwrap_err().contains("role 'user'"));
        assert!(load("nothing", &app, &proj, true).unwrap_err().contains("no theme 'nothing'"));
        assert!(load("../x", &app, &proj, true).unwrap_err().contains("bad theme name"));
        let names = available(&app, &proj);
        assert!(["dark", "light", "mono", "sunset", "broken"].iter().all(|n| names.iter().any(|x| x == n)), "{names:?}");
        assert_eq!(names.iter().filter(|n| *n == "sunset").count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
