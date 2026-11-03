use super::*;
use std::io::Read;
use std::time::Duration;

pub struct WebFetch;

const MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const UA: &str = "Mozilla/5.0 (compatible; moco/0.0; +https://github.com/moco)";

fn is_textual(content_type: &str) -> bool {
    let t = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    t.is_empty()
        || t.starts_with("text/")
        || t.contains("json")
        || t.contains("xml")
        || t.contains("javascript")
        || t.contains("yaml")
        || t == "application/x-www-form-urlencoded"
}

fn is_html(content_type: &str, body: &str) -> bool {
    let t = content_type.to_lowercase();
    t.contains("html") || (t.is_empty() && body.trim_start().starts_with('<'))
}

/// Resolve a `Location` header against the URL it came from.
pub fn join_url(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let origin_end = base[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(base.len());
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{}//{rest}", &base[..scheme_end - 2]);
    }
    if location.starts_with('/') {
        return format!("{}{location}", &base[..origin_end]);
    }
    let dir_end = base[..base.find(['?', '#']).unwrap_or(base.len())].rfind('/').filter(|i| *i >= origin_end).map(|i| i + 1).unwrap_or(origin_end);
    let dir = if dir_end == origin_end { format!("{}/", &base[..origin_end]) } else { base[..dir_end].to_string() };
    format!("{dir}{location}")
}

impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "webfetch"
    }
    fn description(&self) -> String {
        "Fetch a web page or API response over http(s). HTML is converted to Markdown by default (`format`: markdown, text or html). \
Responses over 5 MiB and binary content (images, archives, PDFs) are rejected."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "url":{"type":"string","description":"http:// or https:// URL"},
            "format":{"type":"string","enum":["text","markdown","html"],"description":"Default markdown"},
            "timeout":{"type":"integer","minimum":1,"maximum":120,"description":"Seconds (default 30)"}},"required":["url"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let mut url = str_arg(input, "url")?.trim().to_string();
        let format = opt_str(input, "format").unwrap_or("markdown");
        if !["text", "markdown", "html"].contains(&format) {
            return Err(ToolError::fail("format must be text, markdown or html"));
        }
        let timeout = opt_u64(input, "timeout").unwrap_or(30).clamp(1, 120);
        let (body, content_type) = fetch(ctx, &mut url, Duration::from_secs(timeout), MAX_BYTES)?;
        let text = if is_html(&content_type, &body) {
            match format {
                "html" => body,
                "text" => html::to_text(&body),
                _ => html::to_markdown(&body),
            }
        } else {
            body
        };
        Ok(ToolOutput { text, metadata: json!({"url": url, "contentType": content_type}) })
    }
}

/// Run a blocking network step on a helper thread and wait for it, so a cancel is honored within a moment instead
/// of after the read timeout. An abandoned helper ends by itself when its request times out.
fn blocking<T: Send + 'static>(ctx: &ToolCtx, f: impl FnOnce() -> T + Send + 'static) -> Result<T, ToolError> {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    loop {
        if ctx.cancelled() {
            return Err(ToolError::Interrupted);
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(v) => return Ok(v),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(ToolError::fail("the request thread ended unexpectedly")),
        }
    }
}

/// GET with manual redirects: every hop is permission-checked again. Returns (body, content type).
fn fetch(ctx: &ToolCtx, url: &mut String, timeout: Duration, max_bytes: usize) -> Result<(String, String), ToolError> {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(timeout).build();
    for hop in 0..=MAX_REDIRECTS {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::fail("URL must start with http:// or https://"));
        }
        ctx.permit("webfetch", vec![url.clone()], vec!["*".into()])?;
        if ctx.cancelled() {
            return Err(ToolError::Interrupted);
        }
        let call = agent
            .get(url)
            .set("User-Agent", UA)
            .set("Accept", "text/html,application/xhtml+xml,text/markdown,text/plain,application/json;q=0.9,*/*;q=0.8");
        let resp = blocking(ctx, move || {
            call.call().map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("Request failed with status code: {code}"),
                ureq::Error::Transport(t) => format!("Request failed: {t}"),
            })
        })?
        .map_err(ToolError::fail)?;
        if matches!(resp.status(), 301 | 302 | 303 | 307 | 308) {
            let Some(loc) = resp.header("location") else { return Err(ToolError::fail("Redirect without a Location header")) };
            if hop == MAX_REDIRECTS {
                return Err(ToolError::fail("Too many redirects"));
            }
            *url = join_url(url, loc);
            continue;
        }
        let content_type = resp.header("content-type").unwrap_or("").to_string();
        if !is_textual(&content_type) {
            return Err(ToolError::fail(format!("Unsupported content type: {content_type}. webfetch returns text only.")));
        }
        if resp.header("content-length").and_then(|l| l.parse::<usize>().ok()).is_some_and(|l| l > max_bytes) {
            return Err(ToolError::fail(format!("Response too large (exceeds {max_bytes} byte limit)")));
        }
        let buf = blocking(ctx, move || {
            let mut buf = Vec::new();
            resp.into_reader().take(max_bytes as u64 + 1).read_to_end(&mut buf).map(|_| buf)
        })?
        .map_err(|e| ToolError::fail(format!("Reading the response failed: {e}")))?;
        if buf.len() > max_bytes {
            return Err(ToolError::fail(format!("Response too large (exceeds {max_bytes} byte limit)")));
        }
        return Ok((String::from_utf8_lossy(&buf).into_owned(), content_type));
    }
    Err(ToolError::fail("Too many redirects"))
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;
    use crate::provider::testserver::*;

    fn page(ct: &str, body: &str) -> Canned {
        Canned { status: 200, headers: vec![("Content-Type", ct.into())], body: body.into() }
    }
    fn run(env: &Env, v: Value) -> Result<ToolOutput, ToolError> {
        WebFetch.run(&v, &env.ctx())
    }

    #[test]
    fn html_formats_and_passthrough() {
        let env = Env::new();
        let html = "<html><body><h1>Hi</h1><p>a <a href=\"/x\">link</a></p></body></html>";
        let s = serve(vec![page("text/html; charset=utf-8", html), page("text/html", html), page("text/html", html), page("application/json", "{\"a\":1}")]);
        let u = format!("{}/p", s.url);
        assert_eq!(run(&env, json!({"url":u})).unwrap().text, "# Hi\n\na [link](/x)");
        assert_eq!(run(&env, json!({"url":u,"format":"text"})).unwrap().text, "Hi\n\na link");
        assert_eq!(run(&env, json!({"url":u,"format":"html"})).unwrap().text, html);
        let j = run(&env, json!({"url":u})).unwrap();
        assert_eq!((j.text.as_str(), j.metadata["contentType"].as_str()), ("{\"a\":1}", Some("application/json")), "non-HTML is returned as is");
        let (head, _) = s.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("GET /p ") && head.to_lowercase().contains("user-agent: mozilla/5.0 (compatible; moco"), "{head}");
        assert_eq!(env.perm.asked()[0].action, "webfetch");
    }

    #[test]
    fn redirects_are_followed_and_each_hop_is_checked() {
        let env = Env::new();
        let target = serve(vec![page("text/plain", "final")]);
        let first = serve(vec![Canned { status: 302, headers: vec![("Location", format!("{}/landing", target.url))], body: String::new() }]);
        let o = run(&env, json!({"url": format!("{}/start", first.url)})).unwrap();
        assert_eq!(o.text, "final");
        assert_eq!(o.metadata["url"], format!("{}/landing", target.url));
        let asked: Vec<_> = env.perm.asked().into_iter().map(|r| r.resources[0].clone()).collect();
        assert_eq!(asked, [format!("{}/start", first.url), format!("{}/landing", target.url)]);
        // A denied redirect target stops the fetch.
        let t2 = serve(vec![page("text/plain", "secret")]);
        let f2 = serve(vec![Canned { status: 301, headers: vec![("Location", format!("{}/x", t2.url))], body: String::new() }]);
        *env.perm.reply.lock().unwrap() = Err(ToolError::Blocked("Permission denied".into()));
        assert!(run(&env, json!({"url": format!("{}/s", f2.url)})).is_err());
        assert!(t2.requests.lock().unwrap().is_empty(), "the redirect target was never contacted");
    }

    #[test]
    fn errors_limits_and_bad_input() {
        let env = Env::new();
        let s = serve(vec![error(404, "nope", vec![]), page("image/png", "PNG"), page("text/plain", &"x".repeat(100))]);
        assert_eq!(run(&env, json!({"url": format!("{}/a", s.url)})).unwrap_err().message(), "Request failed with status code: 404");
        assert!(run(&env, json!({"url": format!("{}/b", s.url)})).unwrap_err().message().contains("Unsupported content type: image/png"));
        let mut u = format!("{}/c", s.url);
        let e = fetch(&env.ctx(), &mut u, Duration::from_secs(5), 50).unwrap_err();
        assert!(e.message().contains("Response too large (exceeds 50 byte limit)"), "{}", e.message());
        assert!(run(&env, json!({"url":"ftp://x/y"})).unwrap_err().message().contains("http:// or https://"));
        assert!(run(&env, json!({"url":"https://x","format":"pdf"})).is_err());
        assert!(run(&env, json!({"url":"http://127.0.0.1:1/"})).unwrap_err().message().starts_with("Request failed"));
    }

    #[test]
    fn cancel_ends_a_stalled_fetch_at_once() {
        use std::sync::atomic::Ordering;
        // Headers never arrive, then headers arrive but the body stalls.
        for head in [None, Some("")] {
            let env = Env::new();
            let s = stall(head, Duration::from_secs(30));
            let started = std::time::Instant::now();
            let r = std::thread::scope(|sc| {
                sc.spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    env.cancel.store(true, Ordering::Relaxed);
                });
                run(&env, json!({"url": format!("{}/x", s.url), "timeout": 60}))
            });
            assert!(matches!(r, Err(ToolError::Interrupted)), "{r:?}");
            assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        }
    }

    #[test]
    fn url_joining() {
        assert_eq!(join_url("http://h/a/b?q=1", "c"), "http://h/a/c");
        assert_eq!(join_url("http://h/a/b", "/root"), "http://h/root");
        assert_eq!(join_url("https://h/a/b", "//other/x"), "https://other/x");
        assert_eq!(join_url("http://h", "x"), "http://h/x");
        assert_eq!(join_url("http://h/a", "https://z/y"), "https://z/y");
    }
}
