//! `websearch`: look something up on the web through a configurable provider and return titles, links and
//! snippets (the model reads a page with `webfetch`). Providers: DuckDuckGo's HTML page (needs no key and no
//! account, the default), Brave Search, Tavily and Exa (API keys), and any SearXNG instance.
use super::*;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const MAX_BODY: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// `duckduckgo`, `brave`, `tavily`, `exa` or `searxng`.
    pub provider: String,
    /// Replaces the provider's endpoint (required for `searxng`; handy for proxies and tests).
    pub url: Option<String>,
    pub max_results: usize,
    /// Extra environment variables to read the API key from (after `<PROVIDER>_API_KEY`).
    pub key_env: Vec<String>,
    pub app_dir: PathBuf,
}

impl Settings {
    pub fn default_for(app_dir: PathBuf) -> Settings {
        Settings { provider: "duckduckgo".into(), url: None, max_results: 8, key_env: vec![], app_dir }
    }

    /// From config: `None` when switched off (`"websearch": false`).
    pub fn from_config(cfg: &Option<crate::config::WebSearchSetting>, app_dir: PathBuf) -> Option<Settings> {
        let c = crate::config::WebSearchSetting::resolve(cfg)?;
        let mut s = Settings::default_for(app_dir);
        if let Some(p) = c.provider {
            s.provider = p.to_lowercase();
        }
        s.url = c.url;
        if let Some(n) = c.max_results {
            s.max_results = (n as usize).clamp(1, 20);
        }
        s.key_env = c.key_env;
        Some(s)
    }
}

pub struct WebSearch {
    pub settings: Settings,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes().map(|c| if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) { (c as char).to_string() } else { format!("%{c:02X}") }).collect()
}

/// Markup in a snippet or title to plain text on one line.
fn plain(s: &str) -> String {
    super::html::to_text(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The target of a DuckDuckGo result link: direct, or wrapped in `//duckduckgo.com/l/?uddg=<encoded>`.
fn unwrap_ddg(href: &str) -> Option<String> {
    let href = super::html::decode_entities(href);
    if let Some(i) = href.find("uddg=") {
        let rest = &href[i + 5..];
        return Some(percent_decode(rest.split('&').next().unwrap_or(rest)));
    }
    if href.contains("duckduckgo.com/y.js") || href.contains("duckduckgo.com/aclk") {
        return None; // an ad
    }
    let url = if let Some(r) = href.strip_prefix("//") { format!("https://{r}") } else { href };
    url.starts_with("http").then_some(url)
}

/// Results from DuckDuckGo's HTML endpoint. Ads are skipped.
pub fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    let mut hits = vec![];
    let mut pos = 0;
    while let Some(i) = html[pos..].find("class=\"result__a\"") {
        let at = pos + i;
        // The enclosing result block starts at the previous `class="result ` marker; ads carry `result--ad`.
        let block_start = html[..at].rfind("<div class=\"result ").or_else(|| html[..at].rfind("<div class=\"result\"")).unwrap_or(0);
        let is_ad = html[block_start..at].contains("result--ad");
        let tag_start = html[..at].rfind("<a ").unwrap_or(at);
        let tag_end = html[at..].find('>').map(|e| at + e + 1).unwrap_or(html.len());
        let tag = &html[tag_start..tag_end];
        let href = tag.split("href=\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("");
        let title_end = html[tag_end..].find("</a>").map(|e| tag_end + e).unwrap_or(html.len());
        let title = plain(&html[tag_end..title_end]);
        // The snippet belongs to this result when it comes before the next result link.
        let next = html[title_end..].find("class=\"result__a\"").map(|n| title_end + n).unwrap_or(html.len());
        let snippet = html[title_end..next]
            .find("class=\"result__snippet\"")
            .map(|s| {
                let from = title_end + s;
                let open_end = html[from..].find('>').map(|e| from + e + 1).unwrap_or(from);
                let close = html[open_end..].find("</a>").map(|e| open_end + e).unwrap_or(next);
                plain(&html[open_end..close])
            })
            .unwrap_or_default();
        if !is_ad && !title.is_empty() {
            if let Some(url) = unwrap_ddg(href) {
                hits.push(Hit { title, url, snippet });
            }
        }
        pos = title_end.max(at + 1);
    }
    hits
}

fn jstr(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

pub fn parse_brave(body: &Value) -> Vec<Hit> {
    body["web"]["results"].as_array().into_iter().flatten().map(|r| Hit { title: plain(&jstr(r, "title")), url: jstr(r, "url"), snippet: plain(&jstr(r, "description")) }).collect()
}

pub fn parse_tavily(body: &Value) -> Vec<Hit> {
    body["results"].as_array().into_iter().flatten().map(|r| Hit { title: jstr(r, "title"), url: jstr(r, "url"), snippet: plain(&jstr(r, "content")) }).collect()
}

pub fn parse_exa(body: &Value) -> Vec<Hit> {
    body["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let snippet = r["highlights"].as_array().map(|h| h.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty()).unwrap_or_else(|| jstr(r, "text"));
            Hit { title: jstr(r, "title"), url: jstr(r, "url"), snippet: plain(&snippet) }
        })
        .collect()
}

pub fn parse_searxng(body: &Value) -> Vec<Hit> {
    body["results"].as_array().into_iter().flatten().map(|r| Hit { title: jstr(r, "title"), url: jstr(r, "url"), snippet: plain(&jstr(r, "content")) }).collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}...", s.chars().take(max).collect::<String>()) } else { s.to_string() }
}

impl WebSearch {
    fn key(&self) -> Result<String, ToolError> {
        let p = &self.settings.provider;
        crate::auth::load_key_with(p, &self.settings.key_env, &self.settings.app_dir).ok_or_else(|| {
            ToolError::fail(format!("web search with '{p}' needs an API key: set {} or run `moco auth {p}`.", crate::auth::env_var_for(p)))
        })
    }

    fn read(resp: ureq::Response) -> Result<String, ToolError> {
        let mut buf = Vec::new();
        resp.into_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut buf).map_err(|e| ToolError::fail(format!("Reading the search response failed: {e}")))?;
        if buf.len() > MAX_BODY {
            return Err(ToolError::fail("The search response was too large"));
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn call(&self, req: ureq::Request, body: Option<Value>, form: Option<&[(&str, &str)]>) -> Result<String, ToolError> {
        let result = match (body, form) {
            (Some(b), _) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
            (None, Some(f)) => req.send_form(f),
            (None, None) => req.call(),
        };
        match result {
            Ok(r) => Self::read(r),
            Err(ureq::Error::Status(code, r)) => {
                let detail = Self::read(r).unwrap_or_default();
                let hint = match code {
                    401 | 403 => " (check the API key)",
                    429 => " (rate limited: wait a bit or use another provider)",
                    _ => "",
                };
                Err(ToolError::fail(format!("The search service answered with status {code}{hint}. {}", clip(&plain(&detail), 200))))
            }
            Err(ureq::Error::Transport(t)) => Err(ToolError::fail(format!("The search request failed: {t}"))),
        }
    }

    pub fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, ToolError> {
        let s = &self.settings;
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
        let base = |default: &str| s.url.clone().unwrap_or_else(|| default.to_string());
        let json_of = |text: String| serde_json::from_str::<Value>(&text).map_err(|e| ToolError::fail(format!("The search service sent something that is not JSON: {e}")));
        let hits = match s.provider.as_str() {
            "duckduckgo" | "ddg" => {
                let page = self.call(agent.post(&base("https://html.duckduckgo.com/html/")).set("User-Agent", UA), None, Some(&[("q", query), ("kl", "wt-wt")]))?;
                let hits = parse_duckduckgo(&page);
                if hits.is_empty() && (page.contains("anomaly") || page.contains("captcha") || page.contains("challenge")) {
                    return Err(ToolError::fail("DuckDuckGo asked for a captcha (too many searches from here). Wait a while, or configure another provider with `websearch.provider` (brave, tavily, exa or searxng)."));
                }
                hits
            }
            "brave" => {
                let key = self.key()?;
                let url = format!("{}?q={}&count={count}", base("https://api.search.brave.com/res/v1/web/search"), percent_encode(query));
                parse_brave(&json_of(self.call(agent.get(&url).set("Accept", "application/json").set("X-Subscription-Token", &key), None, None)?)?)
            }
            "tavily" => {
                let key = self.key()?;
                let req = agent.post(&base("https://api.tavily.com/search")).set("Authorization", &format!("Bearer {key}"));
                parse_tavily(&json_of(self.call(req, Some(json!({"query": query, "max_results": count, "include_answer": false})), None)?)?)
            }
            "exa" => {
                let key = self.key()?;
                let req = agent.post(&base("https://api.exa.ai/search")).set("x-api-key", &key);
                parse_exa(&json_of(self.call(req, Some(json!({"query": query, "numResults": count, "contents": {"highlights": {"numSentences": 2}}})), None)?)?)
            }
            "searxng" => {
                let Some(root) = &s.url else { return Err(ToolError::fail("the searxng provider needs `websearch.url` (the address of your SearXNG instance)")) };
                let url = format!("{}/search?q={}&format=json", root.trim_end_matches('/'), percent_encode(query));
                parse_searxng(&json_of(self.call(agent.get(&url).set("Accept", "application/json"), None, None)?)?)
            }
            other => return Err(ToolError::fail(format!("unknown web search provider '{other}' (duckduckgo, brave, tavily, exa or searxng)"))),
        };
        Ok(hits.into_iter().filter(|h| !h.url.is_empty()).take(count).collect())
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> String {
        "Search the web and get back titles, links and short snippets. Use it for current information, library or API documentation, \
and error messages you do not recognize; read a result in full with `webfetch`. Write a focused query (key terms, not a sentence)."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"What to search for"},
            "max_results":{"type":"integer","minimum":1,"maximum":20,"description":"How many results (default from config, 8)"}},"required":["query"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let query = str_arg(input, "query")?.trim().to_string();
        if query.is_empty() {
            return Err(ToolError::fail("`query` must not be empty"));
        }
        let count = opt_u64(input, "max_results").map(|n| n as usize).unwrap_or(self.settings.max_results).clamp(1, 20);
        ctx.permit("websearch", vec![query.clone()], vec!["*".into()])?;
        if ctx.cancelled() {
            return Err(ToolError::Interrupted);
        }
        let hits = self.search(&query, count)?;
        let mut text = format!("Search results for \"{query}\" ({}):\n", self.settings.provider);
        if hits.is_empty() {
            text.push_str("\nNo results. Try different or fewer key terms.");
        }
        for (i, h) in hits.iter().enumerate() {
            text.push_str(&format!("\n{}. {}\n   {}\n", i + 1, if h.title.is_empty() { "(no title)" } else { &h.title }, h.url));
            if !h.snippet.is_empty() {
                text.push_str(&format!("   {}\n", clip(&h.snippet, 400)));
            }
        }
        if !hits.is_empty() {
            text.push_str("\nUse webfetch to read a page.");
        }
        Ok(ToolOutput { text, metadata: json!({"provider": self.settings.provider, "query": query, "results": hits.len()}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;
    use crate::provider::testserver::*;

    fn tool(provider: &str, url: Option<String>) -> WebSearch {
        let mut s = Settings::default_for(std::env::temp_dir().join(format!("moco-ws-{}", std::process::id())));
        s.provider = provider.into();
        s.url = url;
        WebSearch { settings: s }
    }

    fn ok(ct: &str, body: &str) -> Canned {
        Canned { status: 200, headers: vec![("Content-Type", ct.into())], body: body.into() }
    }

    const DDG: &str = r#"<html><body>
<div class="result results_links results_links_deep result--ad"><h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/y.js?ad_domain=shop.example&amp;u3=x">Buy things &amp; more</a></h2>
<a class="result__snippet" href="x">An advertisement</a></div>
<div class="result results_links results_links_deep web-result "><div class="links_main links_deep result__body"><h2 class="result__title">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fureq%2Flatest%2F%3Fa%3D1%26b%3D2&amp;rut=abc">Timeout in <b>ureq</b> - Docs.rs</a></h2>
<a class="result__url" href="x">docs.rs</a>
<a class="result__snippet" href="https://docs.rs/ureq">The various <b>timeouts</b>. Each enum &amp; value.</a></div></div>
<div class="result results_links results_links_deep web-result "><div class="links_main links_deep result__body"><h2 class="result__title">
<a rel="nofollow" class="result__a" href="https://example.com/direct">Direct link</a></h2></div></div>
</body></html>"#;

    #[test]
    fn duckduckgo_results_skip_ads_unwrap_redirects_and_clean_text() {
        let hits = parse_duckduckgo(DDG);
        assert_eq!(
            hits,
            vec![
                Hit { title: "Timeout in ureq - Docs.rs".into(), url: "https://docs.rs/ureq/latest/?a=1&b=2".into(), snippet: "The various timeouts. Each enum & value.".into() },
                Hit { title: "Direct link".into(), url: "https://example.com/direct".into(), snippet: String::new() },
            ]
        );
        assert!(parse_duckduckgo("<html>nothing</html>").is_empty());
    }

    #[test]
    fn json_providers_map_their_fields() {
        let brave = json!({"web": {"results": [{"title": "A <strong>b</strong>", "url": "https://a", "description": "d <strong>x</strong>"}]}});
        assert_eq!(parse_brave(&brave), vec![Hit { title: "A b".into(), url: "https://a".into(), snippet: "d x".into() }]);
        let tavily = json!({"results": [{"title": "T", "url": "https://t", "content": "c", "score": 0.9}]});
        assert_eq!(parse_tavily(&tavily)[0].snippet, "c");
        let exa = json!({"results": [{"title": "E", "url": "https://e", "highlights": ["one", "two"], "text": "long"}, {"title": "F", "url": "https://f", "text": "only text"}]});
        let hits = parse_exa(&exa);
        assert_eq!((hits[0].snippet.as_str(), hits[1].snippet.as_str()), ("one two", "only text"));
        assert_eq!(parse_searxng(&json!({"results": [{"title": "S", "url": "https://s", "content": "c"}]}))[0].url, "https://s");
    }

    #[test]
    fn percent_coding_round_trips() {
        assert_eq!(percent_encode("rust ureq: a&b=é"), "rust%20ureq%3A%20a%26b%3D%C3%A9");
        assert_eq!(percent_decode("a%20b+c%3Fd%"), "a b c?d%");
        assert_eq!(percent_decode("100%zz"), "100%zz");
    }

    #[test]
    fn duckduckgo_search_posts_the_query_and_formats_the_answer() {
        let env = Env::new();
        let s = serve(vec![ok("text/html", DDG)]);
        let out = tool("duckduckgo", Some(format!("{}/html/", s.url))).run(&json!({"query": "ureq timeout", "max_results": 5}), &env.ctx()).unwrap();
        assert!(out.text.starts_with("Search results for \"ureq timeout\" (duckduckgo):\n\n1. Timeout in ureq - Docs.rs\n   https://docs.rs/ureq/latest/?a=1&b=2\n   The various timeouts."), "{}", out.text);
        assert!(out.text.contains("2. Direct link\n   https://example.com/direct\n") && out.text.ends_with("Use webfetch to read a page."), "{}", out.text);
        assert_eq!(out.metadata["results"], 2);
        let (head, body) = s.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /html/ ") && head.to_lowercase().contains("user-agent: mozilla/5.0") && body.contains("q=ureq+timeout"), "{head}\n{body}");
        let asked = env.perm.asked();
        assert_eq!((asked[0].action.as_str(), asked[0].resources[0].as_str(), asked[0].save[0].as_str()), ("websearch", "ureq timeout", "*"));
    }

    #[test]
    fn keyed_providers_send_their_key_and_need_one() {
        let env = Env::new();
        std::env::set_var("BRAVE_API_KEY", "brave-key");
        std::env::set_var("TAVILY_API_KEY", "tav-key");
        let s = serve(vec![ok("application/json", r#"{"web":{"results":[{"title":"B","url":"https://b","description":"d"}]}}"#), ok("application/json", r#"{"results":[{"title":"T","url":"https://t","content":"c"}]}"#)]);
        let out = tool("brave", Some(format!("{}/v1/search", s.url))).run(&json!({"query": "a b&c"}), &env.ctx()).unwrap();
        assert!(out.text.contains("1. B\n   https://b\n   d"), "{}", out.text);
        let (head, _) = s.requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("GET /v1/search?q=a%20b%26c&count=8 ") && head.to_lowercase().contains("x-subscription-token: brave-key"), "{head}");
        let out = tool("tavily", Some(format!("{}/search", s.url))).run(&json!({"query": "q", "max_results": 3}), &env.ctx()).unwrap();
        assert!(out.text.contains("1. T"), "{}", out.text);
        let (head, body) = s.requests.lock().unwrap()[1].clone();
        assert!(head.to_lowercase().contains("authorization: bearer tav-key") && body.contains("\"max_results\":3"), "{head}\n{body}");
        std::env::remove_var("EXA_API_KEY");
        let err = tool("exa", None).run(&json!({"query": "q"}), &env.ctx()).unwrap_err();
        assert!(err.message().contains("needs an API key") && err.message().contains("EXA_API_KEY") && err.message().contains("moco auth exa"), "{}", err.message());
    }

    #[test]
    fn searxng_needs_an_address_and_errors_are_explained() {
        let env = Env::new();
        assert!(tool("searxng", None).run(&json!({"query": "q"}), &env.ctx()).unwrap_err().message().contains("websearch.url"));
        let s = serve(vec![ok("application/json", r#"{"results":[{"title":"S","url":"https://s","content":"hit"}]}"#), error(429, "slow down", vec![]), ok("text/html", "<html>please solve this captcha, anomaly detected</html>"), ok("text/html", "<html>no results at all</html>")]);
        let t = tool("searxng", Some(format!("{}/", s.url)));
        assert!(t.run(&json!({"query": "x y"}), &env.ctx()).unwrap().text.contains("1. S"));
        assert!(s.requests.lock().unwrap()[0].0.starts_with("GET /search?q=x%20y&format=json "));
        let e = t.run(&json!({"query": "x"}), &env.ctx()).unwrap_err();
        assert!(e.message().contains("status 429") && e.message().contains("rate limited"), "{}", e.message());
        let e = tool("duckduckgo", Some(s.url.clone())).run(&json!({"query": "x"}), &env.ctx()).unwrap_err();
        assert!(e.message().contains("captcha"), "{}", e.message());
        let empty = tool("duckduckgo", Some(s.url.clone())).run(&json!({"query": "x"}), &env.ctx()).unwrap();
        assert!(empty.text.contains("No results") && !empty.text.contains("webfetch"));
        assert!(tool("bing", None).run(&json!({"query": "q"}), &env.ctx()).unwrap_err().message().contains("unknown web search provider"));
        assert!(tool("duckduckgo", None).run(&json!({"query": "  "}), &env.ctx()).is_err());
        assert!(tool("duckduckgo", None).run(&json!({}), &env.ctx()).is_err());
    }

    #[test]
    fn a_refused_permission_stops_before_any_request() {
        let env = Env::new();
        *env.perm.reply.lock().unwrap() = Err(ToolError::Blocked("Permission denied".into()));
        let s = serve(vec![ok("text/html", DDG)]);
        assert!(tool("duckduckgo", Some(s.url.clone())).run(&json!({"query": "secret project name"}), &env.ctx()).is_err());
        assert!(s.requests.lock().unwrap().is_empty(), "nothing left the machine");
    }
}
