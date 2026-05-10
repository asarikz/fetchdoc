//! Single-shot loopback HTTP server for the OAuth redirect.
//!
//! We bind `127.0.0.1` on an ephemeral port, serve exactly one request, and
//! shut down. Google redirects the browser to
//! `http://127.0.0.1:<port>/?code=...&state=...`, we capture the query, send
//! the user a small HTML "you can close this tab" page, and return.
//!
//! No web framework involved — this is a purpose-built ~80 lines that reads
//! the request line, parses it as a URL, and writes a fixed response. The
//! browser only ever sees one response, so we don't need keep-alive,
//! routing, or any of the rest of HTTP.

use anyhow::Context;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the browser sent us in the OAuth redirect query string.
#[derive(Debug)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// Bind to `127.0.0.1` on an OS-chosen port. Returns the listener and the
/// `http://127.0.0.1:<port>` URL the caller should pass as `redirect_uri`.
pub async fn bind() -> anyhow::Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding 127.0.0.1:0 for OAuth loopback")?;
    let addr: SocketAddr = listener.local_addr()?;
    let redirect = format!("http://{}", addr);
    Ok((listener, redirect))
}

/// Wait for the OAuth redirect and return the parsed query params.
///
/// `success_message` is rendered into the HTML page the browser sees on
/// completion — useful for "Logged in to Gmail. You can close this tab."
pub async fn accept_one(
    listener: TcpListener,
    success_message: &str,
) -> anyhow::Result<CallbackParams> {
    let (mut socket, _peer) = listener
        .accept()
        .await
        .context("accepting loopback connection")?;

    // Read until end of request headers (CRLF CRLF). The OAuth redirect is
    // a single GET with no body, so we can stop there. 8 KiB is more than
    // enough for any realistic auth code.
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        let n = socket
            .read(&mut chunk)
            .await
            .context("reading loopback request")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            break; // give up reading; we'll fail to parse below
        }
    }

    // First line is `GET /?code=...&state=... HTTP/1.1`.
    let request_line = std::str::from_utf8(&buf)
        .ok()
        .and_then(|s| s.lines().next())
        .ok_or_else(|| anyhow::anyhow!("malformed loopback request"))?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("loopback request missing target"))?;

    let params = parse_query(target);

    // Always send a response so the browser doesn't hang. The page content
    // depends on whether we got a code or an error.
    let body = success_html(success_message, params.error.as_deref());
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("writing loopback response")?;
    socket.shutdown().await.ok();
    Ok(params)
}

fn parse_query(target: &str) -> CallbackParams {
    // `target` looks like `/?code=...&state=...`. Stick a dummy host on it
    // so `url::Url` can parse it as an absolute URL.
    let absolute = format!("http://127.0.0.1{target}");
    let parsed = url::Url::parse(&absolute);
    let mut code = None;
    let mut state = None;
    let mut error = None;
    if let Ok(u) = parsed {
        for (k, v) in u.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }
    }
    CallbackParams { code, state, error }
}

fn success_html(success_message: &str, error: Option<&str>) -> String {
    match error {
        Some(e) => format!(
            "<!doctype html><meta charset=utf-8><title>fetchdoc</title>\
             <body style=\"font-family:-apple-system,sans-serif;padding:2em\">\
             <h1>Authorisation failed</h1><p>{}</p>\
             <p>Return to your terminal for details.</p></body>",
            html_escape(e)
        ),
        None => format!(
            "<!doctype html><meta charset=utf-8><title>fetchdoc</title>\
             <body style=\"font-family:-apple-system,sans-serif;padding:2em\">\
             <h1>{}</h1><p>You can close this tab and return to your terminal.</p></body>",
            html_escape(success_message)
        ),
    }
}

fn html_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect(),
            '&' => "&amp;".chars().collect(),
            '"' => "&quot;".chars().collect(),
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state() {
        let p = parse_query("/?code=ABC&state=XYZ");
        assert_eq!(p.code.as_deref(), Some("ABC"));
        assert_eq!(p.state.as_deref(), Some("XYZ"));
        assert!(p.error.is_none());
    }

    #[test]
    fn parses_error_response() {
        let p = parse_query("/?error=access_denied");
        assert!(p.code.is_none());
        assert_eq!(p.error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn url_decodes_query_values() {
        let p = parse_query("/?code=4%2F0Adeu5BX&state=abc");
        assert_eq!(p.code.as_deref(), Some("4/0Adeu5BX"));
    }

    #[tokio::test]
    async fn end_to_end_loopback_round_trip() {
        let (listener, redirect) = bind().await.unwrap();
        let url = format!("{redirect}/?code=hello&state=world");

        let server = tokio::spawn(async move { accept_one(listener, "Test").await });

        // Use a raw TCP write rather than reqwest so we don't have to wire
        // up an HTTP client just for the test.
        let mut s = tokio::net::TcpStream::connect(redirect.trim_start_matches("http://"))
            .await
            .unwrap();
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            url::Url::parse(&url).unwrap().path().to_string()
                + "?"
                + url::Url::parse(&url).unwrap().query().unwrap_or("")
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        s.read_to_end(&mut response).await.unwrap();
        assert!(
            String::from_utf8_lossy(&response).contains("Test"),
            "response: {}",
            String::from_utf8_lossy(&response)
        );

        let params = server.await.unwrap().unwrap();
        assert_eq!(params.code.as_deref(), Some("hello"));
        assert_eq!(params.state.as_deref(), Some("world"));
    }
}
