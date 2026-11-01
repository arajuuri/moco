//! Renders sample screens to JSON (cells with colors) for a PNG preview: `MOCO_GALLERY=dir cargo test gallery`.
//! `scripts/tui-gallery.py` turns the JSON into images. Not a test of behavior; it only runs when asked.
use super::app::*;
use super::theme::Theme;
use moco_core::provider::Usage;
use moco_core::tools::Todo;
use ratatui::{backend::TestBackend, style::Color, Terminal};
use std::sync::mpsc::channel;

fn color(c: Color) -> String {
    match c {
        Color::Reset => "reset".into(),
        Color::Rgb(r, g, b) => format!("rgb:{r},{g},{b}"),
        Color::Indexed(i) => format!("idx:{i}"),
        other => format!("named:{other:?}"),
    }
}

fn dump(app: &mut App, w: u16, h: u16) -> serde_json::Value {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| render(f, app)).unwrap();
    let b = t.backend().buffer();
    let cells: Vec<serde_json::Value> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| {
            let c = &b[(x, y)];
            serde_json::json!([c.symbol(), color(c.fg), color(c.bg), c.modifier.bits()])
        })
        .collect();
    serde_json::json!({ "w": w, "h": h, "cells": cells })
}

fn ev(app: &mut App, depth: usize, agent: &str, e: Ev) {
    app.update(AppEvent::Engine { depth, agent: agent.into(), ev: e });
}

fn conversation(theme: Theme) -> App {
    let mut a = App::new("anthropic/claude-sonnet-5-5", "build");
    a.theme = theme;
    a.title = Some("Fix the failing parser test".into());
    a.rows.push(Row::Banner { title: "moco 0.0.0".into(), model: "anthropic/claude-sonnet-5-5".into(), agent: "build".into(), cwd: "~/Development/parser".into() });
    a.push_user("The parser test `handles_nested_quotes` fails. Can you find out why and fix it?");
    ev(&mut a, 0, "", Ev::Reasoning("The test name suggests the tokenizer mishandles escaped quotes inside a quoted string. Let me look at the test first, then the tokenizer.".into()));
    ev(&mut a, 0, "", Ev::Text("I'll start by running the test and reading the tokenizer.".into()));
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "shell".into(), summary: "cargo test handles_nested_quotes".into(), detail: None });
    ev(&mut a, 0, "", Ev::ToolDone { name: "shell".into(), ok: true, msg: String::new(), output: "test handles_nested_quotes ... FAILED\n\nleft: [Str(\"a\\\"b\")]\nright: [Str(\"a\"), Str(\"b\")]".into() });
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "grep".into(), summary: "fn tokenize".into(), detail: None });
    ev(&mut a, 0, "", Ev::ToolDone { name: "grep".into(), ok: true, msg: String::new(), output: String::new() });
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "read".into(), summary: "src/parser.rs".into(), detail: None });
    ev(&mut a, 0, "", Ev::ToolDone { name: "read".into(), ok: true, msg: String::new(), output: String::new() });
    ev(&mut a, 0, "", Ev::Text("## What is wrong\n\nThe tokenizer ends a string at the **first** `\"`, even when it is escaped:\n\n- `\\\"` inside a string should be kept as part of the string\n- the closing quote is only the first *unescaped* one\n\n```rust\nif c == '\"' {\n    in_string = !in_string;\n}\n```\n\nI'll make the check aware of the backslash.".into()));
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "edit".into(), summary: "src/parser.rs".into(), detail: Some("@@ -41,5 +41,7 @@\n     for c in chars {\n-        if c == '\"' {\n-            in_string = !in_string;\n+        if c == '\\\\' && in_string {\n+            escaped = !escaped;\n+        } else if c == '\"' && !escaped {\n+            in_string = !in_string;\n         }\n     }".into()) });
    ev(&mut a, 0, "", Ev::ToolDone { name: "edit".into(), ok: true, msg: String::new(), output: String::new() });
    ev(&mut a, 1, "explore", Ev::Tool { id: String::new(), name: "grep".into(), summary: "escaped".into(), detail: None });
    ev(&mut a, 1, "explore", Ev::ToolDone { name: "grep".into(), ok: true, msg: String::new(), output: String::new() });
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "shell".into(), summary: "cargo test".into(), detail: None });
    ev(&mut a, 0, "", Ev::ToolDone { name: "shell".into(), ok: false, msg: "exit code 101: 1 test failed".into(), output: String::new() });
    ev(&mut a, 0, "", Ev::Todos(vec![
        Todo { content: "Reproduce the failure".into(), status: "completed".into() },
        Todo { content: "Fix the escape handling".into(), status: "in_progress".into() },
        Todo { content: "Add a regression test".into(), status: "pending".into() },
    ]));
    ev(&mut a, 0, "", Ev::Usage { usage: Usage { input: 5200, output: 640, cache_read: 31000, cache_write: 0 }, cost: Some(0.0231), session_cost: 0.0231, context: 38_000, limit: Some(200_000) });
    a.update(AppEvent::RunDone(Ok(moco_core::engine::RunOutcome::Idle)));
    a.attention = None;
    a.push_user("Great. Please also add a regression test.");
    ev(&mut a, 0, "", Ev::Text("Adding a test that feeds `\"a\\\"b\"` through the tokenizer.".into()));
    a.running = true;
    ev(&mut a, 0, "", Ev::Tool { id: String::new(), name: "read".into(), summary: "tests/parser.rs".into(), detail: None });
    a.queued.push(Queued { id: "q1".into(), steer: true, text: "also run clippy afterwards".into() });
    a.mcp = vec![("files".into(), "connected: 3 tool(s)".into()), ("web".into(), "needs authorization".into())];
    a.jobs = 1;
    a.job_list = vec![("shl_1".into(), "shell 42s".into())];
    a
}

#[test]
fn gallery() {
    let Ok(dir) = std::env::var("MOCO_GALLERY") else { return };
    std::fs::create_dir_all(&dir).unwrap();
    let mut scenes = serde_json::Map::new();
    for (tname, theme) in [("dark", Theme::dark().truecolor(false)), ("light", Theme::light().truecolor(true)), ("dark256", Theme::dark())] {
        let mut a = conversation(theme.clone());
        scenes.insert(format!("{tname}-chat"), dump(&mut a, 132, 44));
        scenes.insert(format!("{tname}-narrow"), dump(&mut a, 80, 30));
        scenes.insert(format!("{tname}-80x24"), dump(&mut a, 80, 24));

        let mut a = conversation(theme.clone());
        a.running = false;
        let (tx, _rx) = channel();
        a.update(AppEvent::Ask { action: "shell".into(), resources: vec!["cargo test --release".into()], save: vec!["cargo test *".into()], reply: tx });
        scenes.insert(format!("{tname}-permission"), dump(&mut a, 100, 30));

        let mut a = conversation(theme.clone());
        a.running = false;
        a.open_picker(Picker::new(
            PickKind::Model,
            "model",
            ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5", "anthropic/claude-haiku-5-5", "openai/gpt-5", "google/gemini-3-pro"]
                .iter()
                .map(|m| PickItem { label: m.to_string(), detail: "ctx 200.0k  $3/$15 per M".into(), value: m.to_string() })
                .collect(),
            "",
        ));
        scenes.insert(format!("{tname}-picker"), dump(&mut a, 100, 30));

        let mut a = conversation(theme.clone());
        a.running = false;
        a.input = "/re".into();
        a.cursor = 3;
        a.completer = Some(Box::new(|_| vec![
            Item { insert: "/review".into(), label: "/review".into(), detail: "review the working-tree changes".into() },
            Item { insert: "/redo".into(), label: "/redo".into(), detail: "bring an undone turn back".into() },
            Item { insert: "/rename".into(), label: "/rename".into(), detail: "name this session".into() },
        ]));
        a.refresh_popup();
        scenes.insert(format!("{tname}-completion"), dump(&mut a, 100, 30));

        let mut a = App::new("anthropic/claude-sonnet-5-5", "build");
        a.theme = theme.clone();
        a.viewer = Some(Viewer {
            files: vec![ViewFile { path: "src/parser.rs".into(), adds: 4, dels: 2, binary: false, patch: "@@ -41,5 +41,7 @@\n     for c in chars {\n-        if c == '\"' {\n-            in_string = !in_string;\n+        if c == '\\\\' && in_string {\n+            escaped = !escaped;\n+        } else if c == '\"' && !escaped {\n+            in_string = !in_string;\n         }\n     }".into() }],
            scroll: 0,
        });
        scenes.insert(format!("{tname}-diff"), dump(&mut a, 100, 22));

        let mut a = App::new("anthropic/claude-sonnet-5-5", "build");
        a.theme = theme.clone();
        a.push_user("Show me what markdown looks like.");
        a.rows.push(Row::Reasoning("The user wants a tour of the rendering. I will include a heading, nested lists, a quote, a table and a code block.\nThen keep it short.".into()));
        a.rows.push(Row::Assistant("# Markdown tour\n\nSome **bold**, some *italic*, `inline code` and a [link](https://example.com).\n\n## Lists\n\n1. First step\n2. Second step with a very long description that has to wrap onto the next line nicely\n   - nested bullet\n   - another one\n\n> A quote from somewhere that is long enough to wrap around the edge of the terminal window.\n\n### A table\n\n| Tool | Calls | Failed |\n|---|---:|---:|\n| read | 12 | 0 |\n| shell | 4 | 1 |\n\n---\n\n```python\ndef greet(name):\n    return f\"hi {name}\"\n```\n\nDone.".into()));
        scenes.insert(format!("{tname}-markdown"), dump(&mut a, 100, 44));

        let mut a = App::new("anthropic/claude-sonnet-5-5", "build");
        a.theme = theme;
        a.rows.push(Row::Banner { title: "moco 0.0.0".into(), model: "anthropic/claude-sonnet-5-5".into(), agent: "build".into(), cwd: "~/Development/parser".into() });
        scenes.insert(format!("{tname}-empty"), dump(&mut a, 100, 24));
    }
    std::fs::write(std::path::Path::new(&dir).join("scenes.json"), serde_json::Value::Object(scenes).to_string()).unwrap();
}
