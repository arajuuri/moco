use super::*;

pub struct Write;

impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Writes a file to the local filesystem, overwriting if one exists. Missing parent directories are created automatically. \
For partial changes, use the edit tool instead."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = ctx.resolve(str_arg(input, "path")?);
        let content = str_arg(input, "content")?;
        ctx.check_external(path.parent().unwrap_or(&path))?;
        ctx.permit("edit", vec![ctx.display(&path)], vec!["*".into()])?;
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
        std::fs::write(&path, bytes).map_err(|e| ToolError::fail(format!("{}: {e}", path.display())))?;
        Ok(ToolOutput {
            text: format!("{} {}", if existed { "Overwrote" } else { "Created" }, ctx.display(&path)),
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
    fn declined_writes_nothing() {
        let env = Env::new();
        *env.perm.reply.lock().unwrap() = Err(ToolError::Corrected("no".into()));
        assert!(Write.run(&json!({"path":"x.txt","content":"c"}), &env.ctx()).is_err());
        assert!(!env.work().join("x.txt").exists());
    }
}
