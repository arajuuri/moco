//! OAuth 2.1 for remote MCP servers (the MCP authorization spec): discovery of the protected resource and its
//! authorization server (RFC 9728, RFC 8414), dynamic client registration (RFC 7591), authorization code flow with PKCE
//! (S256) and the `resource` parameter (RFC 8707), and token refresh. EXPERIMENTAL: tested against a local server that
//! plays the authorization server; not verified against real services.
//!
//! Tokens and registered clients live in `<app dir>/mcp-auth.json` (mode 0600), keyed by the server's URL.
use crate::config::OAuthConfig;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The redirect URI this client was registered with; reused (same port) so a strict server still accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn path(app_dir: &Path) -> PathBuf {
    app_dir.join("mcp-auth.json")
}

fn read_all(app_dir: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path(app_dir)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

pub fn load(app_dir: &Path, server_url: &str) -> Option<Stored> {
    serde_json::from_value(read_all(app_dir).get(server_url)?.clone()).ok()
}

pub fn save(app_dir: &Path, server_url: &str, s: &Stored) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(app_dir)?;
    let mut all = read_all(app_dir);
    all.insert(server_url.to_string(), serde_json::to_value(s)?);
    let p = path(app_dir);
    let tmp = p.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&Value::Object(all))?.as_bytes())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &p)
}

/// Forget the tokens (and registered client) for a server.
pub fn remove(app_dir: &Path, server_url: &str) -> Result<bool, String> {
    let mut all = read_all(app_dir);
    if all.remove(server_url).is_none() {
        return Ok(false);
    }
    crate::auth::write_private(&path(app_dir), serde_json::to_string_pretty(&Value::Object(all)).unwrap_or_default().as_bytes()).map_err(|e| format!("could not forget the stored tokens ({}): {e}", path(app_dir).display()))?;
    crate::log_info!("mcp", "oauth: stored tokens for {server_url} removed");
    Ok(true)
}

// ---- small helpers -------------------------------------------------------------------------------

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (vec![], 0);
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(q: &str) -> Vec<(String, String)> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map(|(k, v)| (dec(k), dec(v))).unwrap_or((dec(p), String::new()))).collect()
}

fn random_b64(bytes: usize) -> String {
    let raw: Vec<u8> = (0..bytes).map(|_| fastrand::u8(..)).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// PKCE (RFC 7636): a random verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = random_b64(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn origin(url: &str) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let end = url[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(url.len());
    url[..end].to_string()
}

fn path_of(url: &str) -> String {
    let o = origin(url);
    url[o.len()..].split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string()
}

/// The identifier sent as the `resource` parameter: the server URL without query, fragment or a trailing slash.
pub fn canonical_resource(url: &str) -> String {
    format!("{}{}", origin(url), path_of(url))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).redirects(3).build()
}

fn get_json(url: &str) -> Result<Value, String> {
    let r = agent().get(url).set("Accept", "application/json").call().map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| format!("{url}: not JSON ({e})"))
}

// ---- discovery -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes: Option<String>,
}

/// `resource_metadata="..."` from a `WWW-Authenticate: Bearer ...` challenge.
pub fn challenge_param(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let at = header.find(&needle)? + needle.len();
    Some(header[at..].split('"').next()?.to_string())
}

fn well_known_for(issuer: &str, kind: &str) -> Vec<String> {
    let (o, p) = (origin(issuer), path_of(issuer));
    if p.is_empty() {
        vec![format!("{o}/.well-known/{kind}")]
    } else {
        // RFC 8414 puts the path after the well-known segment; OIDC discovery also allows appending to the issuer.
        let mut v = vec![format!("{o}/.well-known/{kind}{p}")];
        if kind == "openid-configuration" {
            v.push(format!("{o}{p}/.well-known/openid-configuration"));
        }
        v
    }
}

/// Find the authorization server for `server_url`: from the challenge's `resource_metadata`, else the well-known
/// protected-resource documents, else assume the MCP server is its own authorization server (earlier spec revisions),
/// else fall back to `/authorize`, `/token`, `/register` on its origin.
pub fn discover(server_url: &str, www_authenticate: Option<&str>, cfg: &OAuthConfig) -> Result<AuthServer, String> {
    let mut scopes = www_authenticate.and_then(|h| challenge_param(h, "scope"));
    let mut issuer = origin(server_url);
    let resource_meta_urls: Vec<String> = match www_authenticate.and_then(|h| challenge_param(h, "resource_metadata")) {
        Some(u) => vec![u],
        None => {
            let (o, p) = (origin(server_url), path_of(server_url));
            let mut v = vec![];
            if !p.is_empty() {
                v.push(format!("{o}/.well-known/oauth-protected-resource{p}"));
            }
            v.push(format!("{o}/.well-known/oauth-protected-resource"));
            v
        }
    };
    for u in &resource_meta_urls {
        if let Ok(m) = get_json(u) {
            if let Some(first) = m["authorization_servers"].as_array().and_then(|a| a.first()).and_then(Value::as_str) {
                issuer = first.to_string();
            }
            if scopes.is_none() {
                scopes = m["scopes_supported"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty());
            }
            break;
        }
    }
    let mut urls: Vec<String> = cfg.auth_server_metadata_url.iter().cloned().collect();
    urls.extend(well_known_for(&issuer, "oauth-authorization-server"));
    urls.extend(well_known_for(&issuer, "openid-configuration"));
    for u in urls {
        if let Ok(m) = get_json(&u) {
            if let (Some(a), Some(t)) = (m["authorization_endpoint"].as_str(), m["token_endpoint"].as_str()) {
                if let Some(methods) = m["code_challenge_methods_supported"].as_array() {
                    if !methods.iter().any(|x| x == "S256") {
                        return Err(format!("the authorization server at {issuer} does not support PKCE with S256"));
                    }
                }
                return Ok(AuthServer { authorization_endpoint: a.into(), token_endpoint: t.into(), registration_endpoint: m["registration_endpoint"].as_str().map(String::from), scopes });
            }
        }
    }
    // No metadata anywhere: the conventional paths.
    let o = origin(&issuer);
    Ok(AuthServer { authorization_endpoint: format!("{o}/authorize"), token_endpoint: format!("{o}/token"), registration_endpoint: Some(format!("{o}/register")), scopes })
}

// ---- registration, authorization, tokens --------------------------------------------------------------

fn register(meta: &AuthServer, redirect_uri: &str) -> Result<(String, Option<String>), String> {
    let endpoint = meta.registration_endpoint.as_ref().ok_or("the authorization server does not support dynamic client registration; set `oauth.client_id` for this server in the config")?;
    let body = json!({"client_name":"moco","redirect_uris":[redirect_uri],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"});
    let r = agent()
        .post(endpoint)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("client registration failed: {e}"))?;
    let v: Value = serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let id = v["client_id"].as_str().ok_or("client registration returned no client_id")?.to_string();
    Ok((id, v["client_secret"].as_str().map(String::from)))
}

fn token_request(endpoint: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    match agent().post(endpoint).set("Accept", "application/json").send_form(form) {
        Ok(r) => serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| format!("token endpoint: not JSON ({e})")),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            Err(format!("token endpoint answered {code}: {}", v["error_description"].as_str().or(v["error"].as_str()).map(String::from).unwrap_or_else(|| body.chars().take(200).collect())))
        }
        Err(e) => Err(format!("token endpoint: {e}")),
    }
}

fn apply_tokens(stored: &mut Stored, v: &Value) -> Result<(), String> {
    stored.access_token = Some(v["access_token"].as_str().ok_or("the token response has no access_token")?.to_string());
    if let Some(r) = v["refresh_token"].as_str() {
        stored.refresh_token = Some(r.to_string());
    }
    stored.expires_at = v["expires_in"].as_u64().map(|s| now() + s);
    Ok(())
}

/// Refresh tokens rotate, so two threads refreshing at once would invalidate each other's: one at a time.
static REFRESH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Use the refresh token. Returns the new access token.
pub fn refresh(app_dir: &Path, server_url: &str) -> Result<String, String> {
    let _one_at_a_time = REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    refresh_locked(app_dir, server_url)
}

fn refresh_locked(app_dir: &Path, server_url: &str) -> Result<String, String> {
    let mut s = load(app_dir, server_url).ok_or("not signed in")?;
    let rt = s.refresh_token.clone().ok_or("no refresh token")?;
    let resource = canonical_resource(server_url);
    let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", rt.as_str()), ("client_id", s.client_id.as_str()), ("resource", resource.as_str())];
    let secret = s.client_secret.clone();
    if let Some(sec) = &secret {
        form.push(("client_secret", sec));
    }
    let v = match token_request(&s.token_endpoint, &form) {
        Ok(v) => v,
        Err(e) => {
            crate::log_warn!("mcp", "oauth: refreshing the token for {server_url} failed: {e}");
            return Err(e);
        }
    };
    apply_tokens(&mut s, &v)?;
    save(app_dir, server_url, &s).map_err(|e| format!("could not store the refreshed token ({}), so you will have to sign in again: {e}", path(app_dir).display()))?;
    crate::log_info!("mcp", "oauth: token for {server_url} refreshed (expires in {} s)", s.expires_at.map_or("?".to_string(), |e| e.saturating_sub(now()).to_string()));
    Ok(s.access_token.clone().unwrap_or_default())
}

/// An access token to send, refreshed first if it expires within a minute. `None` when not signed in or when the
/// token cannot be renewed.
pub fn bearer(app_dir: &Path, server_url: &str) -> Option<String> {
    let s = load(app_dir, server_url)?;
    let token = s.access_token.clone()?;
    if s.expires_at.is_some_and(|e| e <= now() + 60) {
        // Another thread may have refreshed while this one waited: look again under the lock.
        let _one_at_a_time = REFRESH.lock().unwrap_or_else(|e| e.into_inner());
        let s = load(app_dir, server_url)?;
        if s.expires_at.is_some_and(|e| e <= now() + 60) {
            return match s.refresh_token {
                Some(_) => crate::log::LogErr::log_err(refresh_locked(app_dir, server_url), "mcp", "refreshing the OAuth token"),
                None => None,
            };
        }
        return s.access_token;
    }
    Some(token)
}

pub fn authorization_url(meta: &AuthServer, client_id: &str, redirect_uri: &str, challenge: &str, state: &str, scope: Option<&str>, resource: &str) -> String {
    let mut q = vec![("response_type", "code"), ("client_id", client_id), ("redirect_uri", redirect_uri), ("code_challenge", challenge), ("code_challenge_method", "S256"), ("state", state), ("resource", resource)];
    if let Some(s) = scope.filter(|s| !s.is_empty()) {
        q.push(("scope", s));
    }
    let sep = if meta.authorization_endpoint.contains('?') { '&' } else { '?' };
    format!("{}{sep}{}", meta.authorization_endpoint, q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"))
}

/// What a browser redirect delivered.
fn parse_callback(q: &[(String, String)], expected_state: &str) -> Result<String, String> {
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Some(e) = get("error") {
        return Err(format!("the authorization server refused: {e}{}", get("error_description").map(|d| format!(" ({d})")).unwrap_or_default()));
    }
    if get("state") != Some(expected_state) {
        return Err("the response did not match this sign-in attempt (state mismatch); start again".into());
    }
    get("code").map(String::from).ok_or_else(|| "the redirect carried no authorization code".into())
}

/// Accepts what a user may paste when their browser could not reach the local listener: the full redirect URL, its
/// query, or the bare code.
pub fn parse_pasted(text: &str, expected_state: &str) -> Result<String, String> {
    let t = text.trim();
    if let Some((_, q)) = t.split_once('?') {
        return parse_callback(&query(q.split('#').next().unwrap_or("")), expected_state);
    }
    if t.contains("code=") {
        return parse_callback(&query(t), expected_state);
    }
    if t.is_empty() { Err("nothing was pasted".into()) } else { Ok(t.to_string()) }
}

/// Wait for the browser to hit `http://127.0.0.1:<port>/callback`. Answers the request with a small page.
fn wait_callback(listener: &TcpListener, state: &str, timeout: Duration, cancel: &AtomicBool) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let end = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if Instant::now() >= end {
            return Err("timed out waiting for the browser".into());
        }
        let (mut sock, _) = match listener.accept() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
        let (p, q) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let reply = |sock: &mut std::net::TcpStream, status: &str, body: &str| {
            let _ = write!(sock, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        };
        if p != "/callback" {
            reply(&mut sock, "404 Not Found", "not found");
            continue;
        }
        let result = parse_callback(&query(q), state);
        let page = match &result {
            Ok(_) => "<html><body style=\"font-family:sans-serif\"><h3>Signed in to moco</h3>You can close this tab and go back to the terminal.</body></html>".to_string(),
            Err(e) => format!("<html><body style=\"font-family:sans-serif\"><h3>Sign-in failed</h3>{}</body></html>", e.replace('<', "&lt;")),
        };
        reply(&mut sock, "200 OK", &page);
        return result;
    }
}

/// How the sign-in talks to the user. All callbacks are called from the thread running `login`.
pub struct LoginIo<'a> {
    /// Show the authorization URL.
    pub show: &'a dyn Fn(&str),
    /// Try to open it in a browser.
    pub open: &'a dyn Fn(&str),
    /// Ask whether the user wants to paste the redirect URL instead of waiting for the browser; `None` means wait.
    pub paste: &'a dyn Fn() -> Option<String>,
    pub cancel: &'a AtomicBool,
}

/// The `WWW-Authenticate` challenge a server answers an unauthenticated `initialize` with.
pub fn probe(server_url: &str, headers: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let mut req = agent().post(server_url).set("Content-Type", "application/json").set("Accept", "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let body = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":super::client::OFFERED,"capabilities":{},"clientInfo":{"name":"moco","version":crate::version::RELEASE}}});
    match req.send_string(&body.to_string()) {
        Err(ureq::Error::Status(401, r)) => r.header("www-authenticate").map(String::from),
        _ => None,
    }
}

/// The whole sign-in: discover, register (or reuse), authorize in the browser, exchange the code, store the tokens.
pub fn login(app_dir: &Path, server_url: &str, headers: &std::collections::BTreeMap<String, String>, cfg: &OAuthConfig, timeout: Duration, io: &LoginIo) -> Result<(), String> {
    crate::log_info!("mcp", "oauth: sign-in to {server_url} started");
    let www = probe(server_url, headers);
    let meta = discover(server_url, www.as_deref(), cfg).inspect_err(|e| crate::log_warn!("mcp", "oauth: discovery for {server_url} failed: {e}"))?;
    crate::log_info!("mcp", "oauth: authorization endpoint {}, token endpoint {}, registration {}", meta.authorization_endpoint, meta.token_endpoint, if meta.registration_endpoint.is_some() { "available" } else { "not offered" });
    // A client registered earlier is only good with the redirect URI it was registered for, so that port is bound again.
    // If it is taken (or the client was preconfigured) the usual route applies: a fresh registration.
    let previous = load(app_dir, server_url).filter(|s| s.token_endpoint == meta.token_endpoint);
    let reusable = match (&cfg.client_id, &cfg.redirect_uri, &previous) {
        (None, None, Some(p)) => p.redirect_uri.as_deref().and_then(|u| {
            let rest = u.strip_prefix("http://127.0.0.1:")?;
            let port: u16 = rest.split('/').next()?.parse().ok()?;
            Some((TcpListener::bind(("127.0.0.1", port)).ok()?, u.to_string(), p.clone()))
        }),
        _ => None,
    };
    let (listener, redirect_uri, client_id, client_secret, registered_with) = match reusable {
        Some((l, uri, p)) => (l, uri, p.client_id, p.client_secret, p.redirect_uri),
        None => {
            let l = TcpListener::bind(("127.0.0.1", cfg.callback_port.unwrap_or(0))).map_err(|e| format!("cannot listen for the browser's redirect: {e}"))?;
            let port = l.local_addr().map_err(|e| e.to_string())?.port();
            let uri = cfg.redirect_uri.clone().unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
            match &cfg.client_id {
                Some(id) => (l, uri, id.clone(), cfg.client_secret.clone(), None),
                None => {
                    let (id, secret) = register(&meta, &uri).inspect_err(|e| crate::log_warn!("mcp", "oauth: {e}"))?;
                    crate::log_info!("mcp", "oauth: registered a new client ({id})");
                    (l, uri.clone(), id, secret, Some(uri))
                }
            }
        }
    };

    let (verifier, challenge) = pkce();
    let state = random_b64(16);
    let resource = canonical_resource(server_url);
    let scope = cfg.scope.as_deref().or(meta.scopes.as_deref());
    let url = authorization_url(&meta, &client_id, &redirect_uri, &challenge, &state, scope, &resource);
    crate::log_info!("mcp", "oauth: waiting for the browser sign-in (the authorization URL is not logged)");
    (io.show)(&url);
    (io.open)(&url);
    let code = match (io.paste)() {
        Some(text) => parse_pasted(&text, &state),
        None => wait_callback(&listener, &state, timeout, io.cancel),
    }
    .inspect_err(|e| crate::log_warn!("mcp", "oauth: sign-in to {server_url} did not complete: {e}"))?;

    let mut form = vec![("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect_uri.as_str()), ("client_id", client_id.as_str()), ("code_verifier", verifier.as_str()), ("resource", resource.as_str())];
    if let Some(sec) = &client_secret {
        form.push(("client_secret", sec));
    }
    let v = token_request(&meta.token_endpoint, &form).inspect_err(|e| crate::log_warn!("mcp", "oauth: exchanging the code failed: {e}"))?;
    let mut stored = Stored { client_id, client_secret, redirect_uri: registered_with, token_endpoint: meta.token_endpoint.clone(), ..Default::default() };
    apply_tokens(&mut stored, &v)?;
    save(app_dir, server_url, &stored).map_err(|e| format!("could not store the tokens ({}), so you will have to sign in again next time: {e}", path(app_dir).display()))?;
    crate::log_info!("mcp", "oauth: signed in to {server_url}; tokens stored");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_pieces() {
        assert_eq!(origin("https://h.example:8443/a/b?x=1"), "https://h.example:8443");
        assert_eq!(path_of("https://h/a/b/?x=1#f"), "/a/b");
        assert_eq!(path_of("https://h"), "");
        assert_eq!(canonical_resource("https://h/mcp/?x=1#f"), "https://h/mcp");
        assert_eq!(canonical_resource("https://h/"), "https://h");
        assert_eq!(enc("a b&c=d/é~-._"), "a%20b%26c%3Dd%2F%C3%A9~-._");
        assert_eq!(dec("a%20b%26c+d%zz%"), "a b&c d%zz%");
        assert_eq!(query("code=abc&state=x%20y&flag"), [("code".into(), "abc".into()), ("state".into(), "x y".into()), ("flag".into(), "".into())]);
        assert_eq!(challenge_param(r#"Bearer realm="mcp", resource_metadata="https://h/.well-known/x", scope="a b""#, "resource_metadata").as_deref(), Some("https://h/.well-known/x"));
        assert_eq!(challenge_param(r#"Bearer scope="a b""#, "scope").as_deref(), Some("a b"));
        assert_eq!(challenge_param("Bearer", "scope"), None);
    }

    #[test]
    fn pkce_challenge_is_the_s256_of_the_verifier() {
        let (v, c) = pkce();
        assert!(v.len() >= 43 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(c, base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
        assert_ne!(pkce().0, v);
        // RFC 7636 appendix B.
        let rfc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert_eq!(rfc, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn well_known_urls_follow_the_rfcs() {
        assert_eq!(well_known_for("https://auth.example", "oauth-authorization-server"), ["https://auth.example/.well-known/oauth-authorization-server"]);
        assert_eq!(well_known_for("https://auth.example/tenant1", "oauth-authorization-server"), ["https://auth.example/.well-known/oauth-authorization-server/tenant1"]);
        assert_eq!(well_known_for("https://auth.example/t", "openid-configuration"), ["https://auth.example/.well-known/openid-configuration/t", "https://auth.example/t/.well-known/openid-configuration"]);
    }

    #[test]
    fn authorization_url_has_everything_the_spec_requires() {
        let meta = AuthServer { authorization_endpoint: "https://a/authorize".into(), token_endpoint: "https://a/token".into(), registration_endpoint: None, scopes: None };
        let u = authorization_url(&meta, "cid", "http://127.0.0.1:9/callback", "CH", "ST", Some("read write"), "https://h/mcp");
        let q = query(u.split_once('?').unwrap().1);
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("response_type"), get("client_id"), get("redirect_uri"), get("code_challenge"), get("code_challenge_method"), get("state"), get("scope"), get("resource")),
            (Some("code"), Some("cid"), Some("http://127.0.0.1:9/callback"), Some("CH"), Some("S256"), Some("ST"), Some("read write"), Some("https://h/mcp")));
        assert!(authorization_url(&meta, "c", "r", "c", "s", None, "x").find("scope=").is_none());
        let with_q = AuthServer { authorization_endpoint: "https://a/authorize?tenant=1".into(), ..meta };
        assert!(authorization_url(&with_q, "c", "r", "c", "s", None, "x").contains("tenant=1&response_type=code"));
    }

    #[test]
    fn redirects_and_pasted_text_are_validated() {
        let q = |s: &str| query(s);
        assert_eq!(parse_callback(&q("code=abc&state=S"), "S"), Ok("abc".into()));
        assert!(parse_callback(&q("code=abc&state=OTHER"), "S").unwrap_err().contains("state mismatch"));
        assert!(parse_callback(&q("error=access_denied&error_description=no+thanks&state=S"), "S").unwrap_err().contains("access_denied (no thanks)"));
        assert!(parse_callback(&q("state=S"), "S").unwrap_err().contains("no authorization code"));
        assert_eq!(parse_pasted("http://127.0.0.1:5/callback?code=abc%2B1&state=S#frag", "S"), Ok("abc+1".into()));
        assert_eq!(parse_pasted("code=abc&state=S", "S"), Ok("abc".into()));
        assert_eq!(parse_pasted("  rawcode  ", "S"), Ok("rawcode".into()));
        assert!(parse_pasted("http://x/cb?code=a&state=WRONG", "S").is_err());
        assert!(parse_pasted("", "S").is_err());
    }

    #[test]
    fn tokens_are_stored_privately_per_server_and_forgotten_on_request() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(crate::id::new_id("oauth"));
        let s = Stored { client_id: "c".into(), token_endpoint: "https://a/token".into(), access_token: Some("at".into()), refresh_token: Some("rt".into()), expires_at: Some(now() + 3600), ..Default::default() };
        save(&dir, "https://one/mcp", &s).unwrap();
        save(&dir, "https://two/mcp", &Stored { client_id: "d".into(), ..Default::default() }).unwrap();
        assert_eq!(load(&dir, "https://one/mcp").unwrap(), s);
        assert_eq!(load(&dir, "https://two/mcp").unwrap().client_id, "d");
        assert_eq!(std::fs::metadata(path(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(bearer(&dir, "https://one/mcp").as_deref(), Some("at"));
        assert!(remove(&dir, "https://one/mcp") == Ok(true) && remove(&dir, "https://one/mcp") == Ok(false));
        assert!(load(&dir, "https://one/mcp").is_none() && load(&dir, "https://two/mcp").is_some());
        assert_eq!(bearer(&dir, "https://nothing/mcp"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
