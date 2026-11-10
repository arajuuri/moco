use super::*;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
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

/// An address on this machine or its local network rather than on the internet: loopback, private (RFC 1918, IPv6
/// unique local), link-local (169.254.0.0/16, where cloud metadata services answer, and fe80::/10), shared
/// (100.64.0.0/10), unspecified, multicast, broadcast and reserved ranges, also when an IPv6 address embeds one of
/// these (IPv4-mapped or -compatible, NAT64, 6to4).
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && o[1] & 0xfe == 18)
                || o[0] >= 240
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            let v4 = |hi: u16, lo: u16| is_local(IpAddr::V4(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))));
            if let Some(m) = a.to_ipv4_mapped() {
                return is_local(IpAddr::V4(m));
            }
            a.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || s[0] & 0xffc0 == 0xfec0
                // ::a.b.c.d (also :: and ::1), NAT64 64:ff9b::a.b.c.d, 6to4 2002:aabb:ccdd::.
                || (s[..6] == [0; 6] && v4(s[6], s[7]))
                || (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && v4(s[6], s[7]))
                || (s[0] == 0x2002 && v4(s[1], s[2]))
        }
    }
}

/// The addresses a URL's host stands for. A literal address is used as is; a name is looked up (getaddrinfo).
fn resolve(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    Ok((bare, port).to_socket_addrs()?.collect())
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
///
/// A hop whose host is or resolves to a local address (`is_local`) also needs `webfetch.private`, which asks in every
/// mode unless a rule allows it: a fetch is a request sent from inside the user's network, which can reach services
/// that trust that (cloud metadata at 169.254.169.254, routers, admin pages, dev servers) and is the way out for
/// what a prompt injection wants to steal or change. It asks rather than refusing because fetching a local dev
/// server or docs page is a normal thing to want. The connection goes to the addresses that were checked, so a name
/// that resolves differently a moment later (DNS rebinding) cannot slip past.
fn fetch(ctx: &ToolCtx, url: &mut String, timeout: Duration, max_bytes: usize) -> Result<(String, String), ToolError> {
    for hop in 0..=MAX_REDIRECTS {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::fail("URL must start with http:// or https://"));
        }
        ctx.permit("webfetch", vec![url.clone()], vec!["*".into()])?;
        // The URL as ureq will see it (`http://2130706433/` is 127.0.0.1), to look up the host it will connect to.
        let parsed = ureq::get(url.as_str()).request_url().map_err(|e| ToolError::fail(format!("Invalid URL: {e}")))?;
        let parsed = parsed.as_url();
        let (host, port) = (parsed.host_str().unwrap_or_default().to_string(), parsed.port_or_known_default().unwrap_or(80));
        let lookup = host.clone();
        let addrs = blocking(ctx, move || resolve(&lookup, port))?.map_err(|e| ToolError::fail(format!("Request failed: could not resolve {host}: {e}")))?;
        if addrs.is_empty() {
            return Err(ToolError::fail(format!("Request failed: no address for {host}")));
        }
        if addrs.iter().any(|a| is_local(a.ip())) {
            let origin = parsed.origin().ascii_serialization();
            ctx.permit("webfetch.private", vec![url.clone()], vec![origin.clone(), format!("{origin}/*")])?;
        }
        if ctx.cancelled() {
            return Err(ToolError::Interrupted);
        }
        let pinned = addrs;
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(timeout)
            .resolver(move |_: &str| -> std::io::Result<Vec<SocketAddr>> { Ok(pinned.clone()) })
            .build();
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
        let asked: Vec<_> = env.perm.asked().into_iter().map(|r| (r.action, r.resources[0].clone())).collect();
        let (start, landing) = (format!("{}/start", first.url), format!("{}/landing", target.url));
        let hop = |a: &str, u: &str| (a.to_string(), u.to_string());
        // The test servers are on 127.0.0.1, so each hop is also a private fetch.
        assert_eq!(asked, [hop("webfetch", &start), hop("webfetch.private", &start), hop("webfetch", &landing), hop("webfetch.private", &landing)]);
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
    fn local_addresses() {
        let local = [
            "127.0.0.1", "127.8.9.10", "0.0.0.0", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254",
            "100.64.0.1", "100.127.255.254", "192.0.0.170", "198.18.0.1", "224.0.0.1", "255.255.255.255", "240.0.0.1",
            "::1", "::", "fc00::1", "fd12:3456::1", "fe80::1", "fec0::1", "ff02::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254",
            "::127.0.0.1", "64:ff9b::a9fe:a9fe", "2002:c0a8:0101::1", "fd00:ec2::254",
        ];
        let public = ["1.1.1.1", "8.8.8.8", "172.32.0.1", "172.15.255.255", "100.128.0.1", "198.20.0.1", "169.255.0.1", "2606:4700::1111", "::ffff:8.8.8.8", "2002:0808:0808::1"];
        for a in local {
            assert!(is_local(a.parse().unwrap()), "{a} is local");
        }
        for a in public {
            assert!(!is_local(a.parse().unwrap()), "{a} is public");
        }
    }

    /// A permitter that applies real rules and refuses whatever would ask, recording each request with its verdict.
    struct Rules(Vec<crate::permission::Rule>, std::sync::Mutex<Vec<(String, String, crate::permission::Effect)>>);
    impl Permitter for Rules {
        fn check(&self, req: &PermReq) -> Result<(), ToolError> {
            use crate::permission::{decide, Effect};
            let e = decide(&req.action, &req.resources, &self.0, &[]);
            self.1.lock().unwrap().push((req.action.clone(), req.resources[0].clone(), e));
            match e {
                Effect::Allow => Ok(()),
                _ => Err(ToolError::Blocked(format!("{e:?}: {}", req.action))),
            }
        }
    }

    #[test]
    fn local_hosts_ask_even_when_fetches_are_allowed() {
        use crate::permission::{mode_rules, Effect, PermissionMode, Rule};
        let env = Env::new();
        let fetch_with = |rules: Vec<Rule>, url: &str| {
            let p = Rules(rules, Default::default());
            let r = {
                let mut ctx = env.ctx();
                ctx.permitter = &p;
                WebFetch.run(&json!({ "url": url }), &ctx)
            };
            (r, p.1.into_inner().unwrap())
        };
        // Auto mode without a preset allows every fetch, but not one to this machine; the server is never contacted.
        let s = serve(vec![page("text/plain", "local")]);
        let u = format!("{}/x", s.url);
        let (r, log) = fetch_with(mode_rules(PermissionMode::Auto), &u);
        assert!(matches!(r, Err(ToolError::Blocked(_))), "{r:?}");
        assert_eq!(log, [("webfetch".into(), u.clone(), Effect::Allow), ("webfetch.private".into(), u.clone(), Effect::Ask)]);
        // A name is looked up first, and so is the odd spelling of an address that ureq would understand.
        for alias in [u.replace("127.0.0.1", "localhost"), u.replace("127.0.0.1", "2130706433"), u.replace("127.0.0.1", "[::ffff:127.0.0.1]")] {
            let (r, log) = fetch_with(mode_rules(PermissionMode::Auto), &alias);
            assert!(r.is_err() && log.last().unwrap().0 == "webfetch.private", "{alias}: {r:?} {log:?}");
        }
        assert!(s.requests.lock().unwrap().is_empty());
        // A rule can allow a local origin (an "always" answer saves the same pattern).
        let mut rules = mode_rules(PermissionMode::Auto);
        rules.push(Rule::new("webfetch.private", &format!("{}/*", s.url), Effect::Allow));
        assert_eq!(fetch_with(rules, &u).0.unwrap().text, "local");
        // A public page that redirects to the metadata address is stopped at that hop.
        let jump = serve(vec![Canned { status: 302, headers: vec![("Location", "http://169.254.169.254/latest/meta-data/".into())], body: String::new() }]);
        let mut rules = mode_rules(PermissionMode::Auto);
        rules.push(Rule::new("webfetch.private", &format!("{}/*", jump.url), Effect::Allow));
        let (r, log) = fetch_with(rules, &format!("{}/go", jump.url));
        assert!(matches!(r, Err(ToolError::Blocked(_))), "{r:?}");
        assert_eq!(log.last().unwrap(), &("webfetch.private".into(), "http://169.254.169.254/latest/meta-data/".into(), Effect::Ask));
        // What is saved for "always" is the origin.
        let env2 = Env::new();
        let _ = run(&env2, json!({"url": u}));
        let private = env2.perm.asked().into_iter().find(|r| r.action == "webfetch.private").unwrap();
        assert_eq!(private.save, [s.url.clone(), format!("{}/*", s.url)]);
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
