//! The message loop: reads native messages, answers each one in order, and
//! hands downloads to VDM through `forward`.

use std::io::{self, Read, Write};

use serde_json::{Value, json};
use vdm_ipc::Request;
use vdm_proto::NewDownload;

use crate::framing;

/// Answer messages from `input` until it reaches EOF.
///
/// A message that is not JSON, or has an unknown or invalid shape, gets
/// `ok: false` and the loop goes on. A length prefix over the limit cannot be
/// skipped, since its body is unread, so the loop replies and returns. A
/// stream that ends in the middle of a message is an error.
pub fn serve<R, W, F>(input: &mut R, output: &mut W, mut forward: F) -> io::Result<()>
where
    R: Read,
    W: Write,
    F: FnMut(&Request) -> anyhow::Result<()>,
{
    loop {
        let message = match framing::read_message(input) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(()),
            Err(err) if framing::is_frame_too_large(&err) => {
                tracing::warn!(%err, "stopping: the stream cannot be resynchronised");
                send(output, &result_error("message too large"))?;
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                tracing::warn!(%err, "ignoring a message that is not JSON");
                send(output, &result_error("message is not valid JSON"))?;
                continue;
            }
            Err(err) => return Err(err),
        };
        let reply = handle(&message, &mut forward);
        send(output, &reply)?;
    }
}

/// The reply to one decoded message.
pub fn handle<F>(message: &Value, forward: &mut F) -> Value
where
    F: FnMut(&Request) -> anyhow::Result<()>,
{
    let Some(kind) = message.get("type").and_then(Value::as_str) else {
        return result_error("missing message type");
    };
    match kind {
        "ping" => json!({ "type": "pong", "version": env!("CARGO_PKG_VERSION") }),
        "download" => match download_request(message) {
            Ok(request) => match forward(&request) {
                Ok(()) => json!({ "type": "result", "ok": true }),
                Err(err) => {
                    let text = format!("{err:#}");
                    tracing::warn!(error = %text, "cannot hand the download to VDM");
                    result_error(&text)
                }
            },
            Err(message) => result_error(&message),
        },
        _ => result_error("unknown message type"),
    }
}

/// Build the request for a `download` message. Only `url` is required.
fn download_request(message: &Value) -> Result<Request, String> {
    let url = match message.get("url") {
        Some(Value::String(url)) if !url.trim().is_empty() => url.clone(),
        _ => return Err("missing url".into()),
    };
    let download = NewDownload {
        url,
        save_dir: None,
        file_name: optional_string(message, "file_name")?,
        connections: None,
        referrer: optional_string(message, "referrer")?,
        user_agent: optional_string(message, "user_agent")?,
        cookies: optional_string(message, "cookies")?,
        checksum: None,
        queue: None,
    };
    Ok(Request { urls: Vec::new(), show: true, downloads: vec![download] })
}

/// A field that may be absent or null, and is otherwise a string.
fn optional_string(message: &Value, key: &str) -> Result<Option<String>, String> {
    match message.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("\"{key}\" must be a string")),
    }
}

fn result_error(message: &str) -> Value {
    json!({ "type": "result", "ok": false, "error": message })
}

/// Write `reply`. A reply over the size limit is replaced by a short error.
fn send<W: Write>(output: &mut W, reply: &Value) -> io::Result<()> {
    match framing::write_message(output, reply) {
        Err(err) if err.kind() == io::ErrorKind::InvalidData => {
            framing::write_message(output, &result_error("reply too large"))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(_: &Request) -> anyhow::Result<()> {
        Ok(())
    }

    #[test]
    fn ping_reports_the_version() {
        let reply = handle(&json!({"type": "ping"}), &mut accept);
        assert_eq!(reply, json!({"type": "pong", "version": env!("CARGO_PKG_VERSION")}));
    }

    #[test]
    fn download_becomes_a_request_with_every_field() {
        let mut seen = Vec::new();
        let mut record = |request: &Request| {
            seen.push(request.clone());
            Ok(())
        };
        let message = json!({
            "type": "download",
            "url": "https://example.com/a.iso",
            "referrer": "https://example.com/",
            "cookies": null,
            "user_agent": "UA",
            "file_name": "a.iso",
        });
        assert_eq!(handle(&message, &mut record), json!({"type": "result", "ok": true}));
        assert_eq!(
            seen,
            vec![Request {
                urls: vec![],
                show: true,
                downloads: vec![NewDownload {
                    url: "https://example.com/a.iso".into(),
                    save_dir: None,
                    file_name: Some("a.iso".into()),
                    connections: None,
                    referrer: Some("https://example.com/".into()),
                    user_agent: Some("UA".into()),
                    cookies: None,
                    checksum: None,
                    queue: None,
                }],
            }]
        );
    }

    #[test]
    fn forward_failure_is_reported_as_not_ok() {
        let mut fail = |_: &Request| Err(anyhow::anyhow!("VDM is down"));
        let reply = handle(&json!({"type": "download", "url": "https://x/a"}), &mut fail);
        assert_eq!(reply, json!({"type": "result", "ok": false, "error": "VDM is down"}));
    }

    #[test]
    fn bad_messages_are_refused() {
        for message in [
            json!({"type": "nope"}),
            json!({"type": "download"}),
            json!({"type": "download", "url": "  "}),
            json!({"type": "download", "url": 5}),
            json!({"type": "download", "url": "https://x/a", "cookies": 1}),
            json!({"url": "https://x/a"}),
            json!([1, 2]),
        ] {
            let reply = handle(&message, &mut accept);
            assert_eq!(reply["type"], "result", "{message}");
            assert_eq!(reply["ok"], false, "{message}");
            assert!(reply["error"].as_str().is_some_and(|e| !e.is_empty()), "{message}");
        }
    }
}
