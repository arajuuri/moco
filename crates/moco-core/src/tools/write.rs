use super::*;

pub struct Write;

impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> String {
        "Writes a file to the local filesystem, overwriting if one exists. Missing parent directories are created automatically. \
For partial changes, use the edit tool instead.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = ctx.resolve(str_arg(input, "path")?);
        let content = str_arg(input, "content")?;
        ctx.check_external_file(&path)?;
        ctx.permit("edit", vec![ctx.display(&path)], vec!["*".into()])?;
        // No other call may change this file between the read below and the end of formatting (issue #6).
        let _locks = super::fsio::lock_files([&path]);
        let existing = std::fs::read(&path).ok();
        let existed = existing.is_some();
        let bom = existing.as_deref().is_some_and(|b| b.starts_with(&[0xef, 0xbb, 0xbf]));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| ToolError::fail(e.to_string()))?;
        }
        let mut bytes = Vec::with_capacity(content.len() + 3);
        if bom && !content.starts_with('\u{feff}') {
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        bytes.extend_from_slice(content.as_bytes());
        super::fsio::write_atomic(&path, &bytes).map_err(|e| ToolError::fail(format!("{}: {e}", path.display())))?;
        let note = ctx.format_file(&path, &bytes).map(|n| format!("\n{n}")).unwrap_or_default();
        Ok(ToolOutput {
            text: format!("{} {}{note}", if existed { "Overwrote" } else { "Created" }, ctx.display(&path)),
            metadata: json!({"path": path.to_string_lossy(), "existed": existed}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    #[test]
    fn creates_dirs_overwrites_and_keeps_bom() {
        let env = Env::new();
        let o = Write.run(&json!({"path":"a/b/c.txt","content":"hi"}), &env.ctx()).unwrap();
        assert_eq!(o.text, "Created a/b/c.txt");
        assert_eq!(std::fs::read_to_string(env.work().join("a/b/c.txt")).unwrap(), "hi");
        let o = Write.run(&json!({"path":"a/b/c.txt","content":"yo"}), &env.ctx()).unwrap();
        assert_eq!((o.text.as_str(), &o.metadata["existed"]), ("Overwrote a/b/c.txt", &json!(true)));
        env.file("bom.txt", b"\xef\xbb\xbfold");
        Write.run(&json!({"path":"bom.txt","content":"new"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read(env.work().join("bom.txt")).unwrap(), b"\xef\xbb\xbfnew");
        let asked = env.perm.asked();
        assert_eq!((asked[0].action.as_str(), asked[0].resources.clone()), ("edit", vec!["a/b/c.txt".to_string()]));
    }

    #[test]
    fn the_formatter_runs_on_what_was_written_and_the_model_is_told() {
        let mut env = Env::new();
        env.formatters = crate::format::tests::upcase_formatters();
        let o = Write.run(&json!({"path":"notes.txt","content":"hello"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(env.work().join("notes.txt")).unwrap(), "HELLO");
        assert!(o.text.starts_with("Created notes.txt\nFormatted with upcase:") && o.text.contains("these lines") && o.text.contains("+1: HELLO"), "{}", o.text);
        // A file the formatter does not cover, and one already in shape, get no note.
        assert_eq!(Write.run(&json!({"path":"code.rs","content":"fn x(){}"}), &env.ctx()).unwrap().text, "Created code.rs");
        assert_eq!(Write.run(&json!({"path":"loud.txt","content":"ALREADY"}), &env.ctx()).unwrap().text, "Created loud.txt");
        // Outside the project nothing is run, even for a covered extension.
        let outside = env.dir.join("elsewhere.txt");
        let o = Write.run(&json!({"path": outside.to_string_lossy(), "content":"quiet"}), &env.ctx()).unwrap();
        assert_eq!((std::fs::read_to_string(&outside).unwrap().as_str(), o.text.contains("Formatted")), ("quiet", false));
    }

    #[test]
    fn a_failing_formatter_does_not_fail_the_write() {
        let mut env = Env::new();
        env.formatters = crate::format::tests::failing_formatters();
        let o = Write.run(&json!({"path":"a.txt","content":"as written"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(env.work().join("a.txt")).unwrap(), "as written");
        assert!(o.text.contains("The formatter strict failed (exit code 1: parse error at 1:1); the file was left as written."), "{}", o.text);
    }

    #[test]
    fn declined_writes_nothing() {
        let env = Env::new();
        *env.perm.reply.lock().unwrap() = Err(ToolError::Corrected("no".into()));
        assert!(Write.run(&json!({"path":"x.txt","content":"c"}), &env.ctx()).is_err());
        assert!(!env.work().join("x.txt").exists());
    }
}
