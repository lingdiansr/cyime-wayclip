use crate::protocol::clipboard_wire::{parse_clipboard_body, ClipboardRequestError};
use crate::protocol::valid_basic;
use crate::{
    protocol::{GetResult, PutResult},
    server::Server,
};
use sha2::Digest;
use std::io::Read;
fn vlog(level: u8, msg: impl std::fmt::Display) {
    crate::logging::log(level, msg);
}
pub fn serve_v1(addr: &str, mut state: Server<()>, pairing_code: &str) -> std::io::Result<()> {
    let server = tiny_http::Server::http(addr).map_err(std::io::Error::other)?;
    for mut req in server.incoming_requests() {
        crate::logging::log(3, format!("v1 request {} {}", req.method(), req.url()));
        let url = req.url().to_string();
        let (status, body, etag) =
            if url == "/api/clipboard" && req.method() == &tiny_http::Method::Get {
                match state.get(None) {
                    GetResult::Profile(p) => (200, serde_json::to_vec(&p).unwrap(), Some(p.hash)),
                    GetResult::NoContent => (204, vec![], None),
                    GetResult::Unavailable => (503, vec![], None),
                    GetResult::Forbidden => (403, vec![], None),
                    GetResult::NotModified => (304, vec![], None),
                }
            } else if url == "/api/clipboard" && req.method() == &tiny_http::Method::Put {
                let mut b = Vec::new();
                req.as_reader().take(262145).read_to_end(&mut b)?;
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| valid_basic(h.value.as_str(), pairing_code))
                    .unwrap_or(false);
                match state.put(&b, auth) {
                    PutResult::Accepted(p) => (200, serde_json::to_vec(&p).unwrap(), None),
                    PutResult::Unauthorized => (401, vec![], None),
                    PutResult::Forbidden => (403, vec![], None),
                    PutResult::PayloadTooLarge => (413, vec![], None),
                    _ => (400, vec![], None),
                }
            } else {
                (404, vec![], None)
            };
        let mut r = tiny_http::Response::from_data(body).with_status_code(status);
        if let Some(e) = etag {
            r.add_header(tiny_http::Header::from_bytes(&b"ETag"[..], e.as_bytes()).unwrap());
        }
        req.respond(r)?;
    }
    Ok(())
}

pub fn serve_v2_tls(
    addr: &str,
    _state: Server<()>,
    identity: &crate::tls::Identity,
    pairing: &mut crate::protocol::PairingService,
) -> std::io::Result<()> {
    vlog(1, format!("starting HTTPS listener on {addr}"));
    let server = tiny_http::Server::https(
        addr,
        tiny_http::SslConfig {
            certificate: identity.cert_pem.clone(),
            private_key: identity.key_pem.clone(),
        },
    )
    .map_err(std::io::Error::other)?;
    serve_v2_requests(
        server.incoming_requests(),
        pairing,
        read_clipboard,
        write_clipboard,
    )
}

fn read_clipboard() -> Result<String, String> {
    #[cfg(feature = "wayland")]
    return crate::clipboard::wayland::paste_text();
    #[cfg(not(feature = "wayland"))]
    Err("Wayland support is disabled".into())
}

fn write_clipboard(text: &str) -> Result<(), String> {
    #[cfg(feature = "wayland")]
    return crate::clipboard::wayland::copy_text(text);
    #[cfg(not(feature = "wayland"))]
    {
        let _ = text;
        Err("Wayland support is disabled".into())
    }
}

// Commit the cache and acknowledge a PUT only after the desktop accepted it.
fn apply_clipboard_put(
    body: &[u8],
    clipboard: &mut Option<serde_json::Value>,
    counter: &mut u64,
    write: impl FnOnce(&str) -> Result<(), String>,
) -> Result<serde_json::Value, ClipboardRequestError> {
    let value = parse_clipboard_body(body)?;
    let text = value["text"].as_str().expect("validated clipboard text");
    write(text).map_err(|e| {
        eprintln!("Wayland clipboard write failed: {e}");
        ClipboardRequestError {
            status: 503,
            reason: "desktop clipboard unavailable",
        }
    })?;
    *counter = counter.saturating_add(1);
    *clipboard = Some(value.clone());
    Ok(value)
}

fn serve_v2_requests(
    requests: impl Iterator<Item = tiny_http::Request>,
    pairing: &mut crate::protocol::PairingService,
    mut read: impl FnMut() -> Result<String, String>,
    mut write: impl FnMut(&str) -> Result<(), String>,
) -> std::io::Result<()> {
    let mut token: Option<String> = None;
    let device_id = uuid::Uuid::new_v4().to_string();
    let mut counter: u64 = 0;
    let mut clipboard: Option<serde_json::Value> = None;
    for mut req in requests {
        let path = req.url().to_string();
        vlog(2, format!("request method={:?} path={path}", req.method()));
        crate::logging::log(3, format!("v2 request {:?} {}", req.method(), path));
        if path == "/api/pair" {
            let mut b = Vec::new();
            req.as_reader().read_to_end(&mut b)?;
            let secret = extract_pair_secret(&b);
            eprintln!("pair request method={:?} path={} content_type={:?} body_len={} body_hash={} secret_len={} secret_hash={}", req.method(), path, req.headers().iter().find(|h| h.field.equiv("Content-Type")).map(|h| h.value.as_str()), b.len(), hex::encode(sha2::Sha256::digest(&b)), secret.len(), hex::encode(sha2::Sha256::digest(secret.as_bytes())));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expected_hash = pairing.expected_secret_hash();
            eprintln!(
                "pair timing now={} expected_secret_hash={} extracted_hash={}",
                now,
                expected_hash,
                hex::encode(sha2::Sha256::digest(secret.as_bytes()))
            );
            let r = pairing.pair(&secret, uuid::Uuid::new_v4().to_string(), now);
            let (code, body) = match r {
                Ok(x) => {
                    eprintln!(
                        "pair accepted status=201 server_id={} device_id={}",
                        x.server_id, x.device_id
                    );
                    (
                        {
                            token = Some(x.access_token.clone());
                            201
                        },
                        serde_json::to_vec(&x).unwrap(),
                    )
                }
                Err(e) => {
                    eprintln!("pair rejected: {e}");
                    (401, vec![])
                }
            };
            req.respond(tiny_http::Response::from_data(body).with_status_code(code))?;
        } else if path == "/api/clipboard" {
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
                .map(|v| token.as_deref() == Some(v.trim()))
                .unwrap_or(false);
            if !auth {
                req.respond(tiny_http::Response::empty(401))?;
                continue;
            }
            if req.method() == &tiny_http::Method::Get {
                if let Ok(text) = read() {
                    let hash = hex::encode(sha2::Sha256::digest(text.as_bytes()));
                    let current = serde_json::json!({"type":"text","text":text,"hash":hash,"size":text.len(),"has_data":false,"data_name":null,"source":"cyime-wayland","v":2,"stamp":{"device_id":device_id,"counter":counter}});
                    clipboard = Some(current);
                }
                if let Some(v) = clipboard.as_ref() {
                    let hash = v.get("hash").and_then(|x| x.as_str()).unwrap_or("");
                    let inm = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("If-None-Match"))
                        .map(|h| {
                            h.value
                                .as_str()
                                .trim_matches('"')
                                .split(',')
                                .any(|x| x.trim().trim_matches('"') == hash)
                        });
                    if inm == Some(true) {
                        let mut resp = tiny_http::Response::empty(304);
                        resp.add_header(
                            tiny_http::Header::from_bytes(&b"ETag"[..], hash.as_bytes()).unwrap(),
                        );
                        req.respond(resp)?;
                    } else {
                        let mut resp =
                            tiny_http::Response::from_string(v.to_string()).with_status_code(200);
                        resp.add_header(
                            tiny_http::Header::from_bytes(&b"ETag"[..], hash.as_bytes()).unwrap(),
                        );
                        req.respond(resp)?;
                    }
                } else {
                    req.respond(tiny_http::Response::empty(204))?;
                }
            } else if req.method() == &tiny_http::Method::Put {
                let mut body = Vec::new();
                req.as_reader()
                    .take((crate::protocol::MAX_JSON_BYTES + 1) as u64)
                    .read_to_end(&mut body)?;
                let v = match apply_clipboard_put(&body, &mut clipboard, &mut counter, &mut write) {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!(
                            "clipboard PUT rejected status={} reason={}",
                            error.status, error.reason
                        );
                        // Android parses any nonempty response as JSON before checking
                        // its HTTP status. Keep errors empty so TransferFailure retains
                        // the actual 400/413/503 rather than becoming a JSONException.
                        req.respond(tiny_http::Response::empty(error.status))?;
                        continue;
                    }
                };
                vlog(
                    1,
                    format!("clipboard PUT applied status=200 bytes={}", v["size"]),
                );
                let mut resp =
                    tiny_http::Response::from_string(v.to_string()).with_status_code(200);
                if let Some(h) = v.get("hash").and_then(|x| x.as_str()) {
                    resp.add_header(
                        tiny_http::Header::from_bytes(&b"ETag"[..], h.as_bytes()).unwrap(),
                    );
                }
                req.respond(resp)?;
            } else {
                req.respond(tiny_http::Response::empty(405))?;
            }
        } else if path == "/api/peer" || path == "/api/grant" || path == "/api/introduce" {
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
                .map(|v| token.as_deref() == Some(v.trim()))
                .unwrap_or(false);
            if !auth {
                req.respond(tiny_http::Response::empty(401))?;
                continue;
            }
            let mut b = Vec::new();
            req.as_reader().read_to_end(&mut b)?;
            let body = if path == "/api/grant" {
                serde_json::json!({"v":2,"token":token.clone().unwrap_or_default()}).to_string()
            } else {
                serde_json::json!({"v":2,"push":false}).to_string()
            };
            req.respond(tiny_http::Response::from_string(body).with_status_code(200))?;
        } else if path == "/api/status" {
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
                .map(|v| token.as_deref() == Some(v.trim()))
                .unwrap_or(false);
            if !auth {
                req.respond(tiny_http::Response::empty(401))?;
                continue;
            }
            req.respond(tiny_http::Response::from_string(r#"{"v":2,"connected":true,"send_enabled":true,"receive_enabled":true,"push":false,"push_pin":null,"push_port":null,"push_failed":false}"#).with_status_code(200))?;
        } else {
            req.respond(tiny_http::Response::empty(404))?;
        }
    }
    Ok(())
}

fn extract_pair_secret(b: &[u8]) -> String {
    let raw = String::from_utf8_lossy(b).trim().to_string();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) {
        if let Some(s) = v.get("secret").and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    for part in raw.split('&') {
        if let Some(v) = part.strip_prefix("secret=") {
            return v.to_string();
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClipboardProfile, PairOffer, PairingService};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn android_profile(text: &str) -> Value {
        let mut value = serde_json::to_value(ClipboardProfile::text(text).unwrap()).unwrap();
        value["source"] = json!("CyIME-Android");
        value["stamp"] = json!(1_793_713_000_000_i64);
        value
    }

    #[test]
    fn failed_write_does_not_acknowledge_or_replace_cached_clipboard() {
        let old = android_profile("desktop");
        let incoming = android_profile("phone 手机");
        let mut clipboard = Some(old.clone());
        let mut counter = 4;
        let error = apply_clipboard_put(
            &serde_json::to_vec(&incoming).unwrap(),
            &mut clipboard,
            &mut counter,
            |_| Err("test unavailable provider".into()),
        )
        .unwrap_err();
        assert_eq!(error.status, 503);
        assert_eq!(clipboard, Some(old));
        assert_eq!(counter, 4);
    }

    #[test]
    fn successful_write_preserves_android_timestamp_and_profile() {
        let incoming = android_profile("手机复制\r\n🌍");
        let mut clipboard = None;
        let mut counter = 0;
        let result = apply_clipboard_put(
            &serde_json::to_vec(&incoming).unwrap(),
            &mut clipboard,
            &mut counter,
            |text| {
                assert_eq!(text, "手机复制\r\n🌍");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result, incoming);
        assert_eq!(clipboard, Some(incoming));
        assert_eq!(counter, 1);
    }

    #[test]
    fn invalid_request_never_calls_desktop_writer() {
        let mut clipboard = None;
        let mut counter = 0;
        let error = apply_clipboard_put(b"{", &mut clipboard, &mut counter, |_| {
            panic!("invalid request must not change desktop clipboard")
        })
        .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(clipboard, None);
        assert_eq!(counter, 0);
    }

    // Exercise the same request loop as TLS, without an actual desktop or phone.
    // In particular, malformed JSON must not terminate subsequent polling/PUTs.
    #[test]
    fn android_upload_over_http_survives_bad_json_and_enforces_auth() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let desktop = Arc::new(Mutex::new(None::<String>));
        let observed = Arc::clone(&desktop);
        let worker = std::thread::spawn(move || {
            let secret = crate::protocol::encode_secret(&[7; 32]).unwrap();
            let mut pairing = PairingService::new("test-server", "test");
            pairing.set_offer(PairOffer {
                v: 2,
                server_id: "test-server".into(),
                name: "test".into(),
                certificate_sha256: "a".repeat(64),
                pairing_id: "test-offer".into(),
                secret,
                endpoints: vec![format!("https://{addr}")],
                expires_at: u64::MAX,
            });
            let read_desktop = Arc::clone(&desktop);
            serve_v2_requests(
                server.incoming_requests().take(10),
                &mut pairing,
                move || read_desktop.lock().unwrap().clone().ok_or("empty".into()),
                move |text| {
                    if text == "WRITE_FAIL" {
                        return Err("test unavailable clipboard".into());
                    }
                    *desktop.lock().unwrap() = Some(text.to_owned());
                    Ok(())
                },
            )
            .unwrap();
        });
        let exchange = |method: &str, path: &str, token: &str, body: &[u8]| {
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            stream.write_all(body).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            let code = headers
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u16>()
                .unwrap();
            (code, body.to_owned())
        };
        let secret = crate::protocol::encode_secret(&[7; 32]).unwrap();
        let (code, response) = exchange(
            "POST",
            "/api/pair",
            "",
            json!({"secret": secret}).to_string().as_bytes(),
        );
        assert_eq!(code, 201);
        let pair: Value = serde_json::from_str(&response).unwrap();
        let token = pair["access_token"].as_str().unwrap();
        assert_eq!(exchange("GET", "/api/status", token, b"").0, 200);
        assert_eq!(
            exchange("PUT", "/api/clipboard", token, b"{"),
            (400, String::new())
        );
        let phone = android_profile("PHONE_TO_PC 手机🌍");
        let body = serde_json::to_vec(&phone).unwrap();
        assert_eq!(exchange("PUT", "/api/clipboard", "wrong", &body).0, 401);
        let (code, response) = exchange("PUT", "/api/clipboard", token, &body);
        assert_eq!(code, 200);
        assert_eq!(serde_json::from_str::<Value>(&response).unwrap(), phone);
        assert_eq!(
            observed.lock().unwrap().as_deref(),
            Some("PHONE_TO_PC 手机🌍")
        );
        let (code, response) = exchange("GET", "/api/clipboard", token, b"");
        assert_eq!(code, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["hash"],
            phone["hash"]
        );
        let mut legacy = android_profile("legacy object stamp");
        legacy["v"] = json!(2);
        legacy["stamp"] = json!({"device_id": "test", "counter": 3});
        assert_eq!(
            exchange(
                "PUT",
                "/api/clipboard",
                token,
                legacy.to_string().as_bytes()
            )
            .0,
            200
        );
        assert_eq!(exchange("GET", "/api/status", token, b"").0, 200);
        let rejected = android_profile("WRITE_FAIL");
        assert_eq!(
            exchange(
                "PUT",
                "/api/clipboard",
                token,
                rejected.to_string().as_bytes()
            ),
            (503, String::new())
        );
        let (code, response) = exchange("GET", "/api/clipboard", token, b"");
        assert_eq!(code, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["hash"],
            legacy["hash"]
        );
        worker.join().unwrap();
    }
}
