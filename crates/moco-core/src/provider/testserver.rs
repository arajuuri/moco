//! Minimal one-shot HTTP server for adapter tests: serves canned responses in order and records requests.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct Canned {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

pub fn sse(body: &str) -> Canned {
    Canned { status: 200, headers: vec![("Content-Type", "text/event-stream".into())], body: body.into() }
}

pub fn error(status: u16, body: &str, headers: Vec<(&'static str, String)>) -> Canned {
    Canned { status, headers, body: body.into() }
}

pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<(String, String)>>>, // (headers+request line, body)
}

pub fn serve(responses: Vec<Canned>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let log = requests.clone();
    std::thread::spawn(move || {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, mut body) = loop {
                let n = sock.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (String::from_utf8_lossy(&buf).into_owned(), Vec::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (String::from_utf8_lossy(&buf[..pos]).into_owned(), buf[pos + 4..].to_vec());
                }
            };
            let want: usize = head
                .lines()
                .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                .unwrap_or(0);
            while body.len() < want {
                let n = sock.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            log.lock().unwrap().push((head, String::from_utf8_lossy(&body).into_owned()));
            let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, resp.body.len());
            for (k, v) in &resp.headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&resp.body);
            let _ = sock.write_all(out.as_bytes());
        }
    });
    Server { url, requests }
}
