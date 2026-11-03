use crate::protocol::valid_basic;
use crate::{
    protocol::{GetResult, PutResult},
    server::Server,
};
use sha2::Digest;
use std::io::Read;
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
    let server = tiny_http::Server::https(
        addr,
        tiny_http::SslConfig {
            certificate: identity.cert_pem.clone(),
            private_key: identity.key_pem.clone(),
        },
    )
    .map_err(std::io::Error::other)?;
    let mut token: Option<String> = None;
    let mut clipboard: Option<serde_json::Value> = None;
    for mut req in server.incoming_requests() {
        let path = req.url().to_string();
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
                #[cfg(feature = "wayland")]
                if let Ok(text) = crate::clipboard::wayland::paste_text() {
                    let hash = hex::encode(sha2::Sha256::digest(text.as_bytes()));
                    let current = serde_json::json!({"type":"text","text":text,"hash":hash,"size":text.len(),"has_data":false,"data_name":null,"source":"cyime-wayland","v":2,"stamp":{"device_id":"linux","counter":0}});
                    clipboard = Some(current);
                }
                if let Some(v) = clipboard.as_ref() {
                    let hash = v.get("hash").and_then(|x| x.as_str()).unwrap_or("");
                    let inm = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("If-None-Match"))
                        .map(|h| h.value.as_str().trim_matches('"'));
                    if inm == Some(hash) {
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
                let mut b = Vec::new();
                req.as_reader().read_to_end(&mut b)?;
                let v: serde_json::Value = serde_json::from_slice(&b)
                    .map_err(|_| std::io::Error::other("invalid clipboard JSON"))?;
                let valid = v.get("v").and_then(|x| x.as_u64()) == Some(2)
                    && v.get("type").and_then(|x| x.as_str()) == Some("text")
                    && v.get("text").and_then(|x| x.as_str()).is_some()
                    && v.get("hash")
                        .and_then(|x| x.as_str())
                        .map(|x| x.len() == 64)
                        .unwrap_or(false)
                    && v.get("size").and_then(|x| x.as_u64()).is_some()
                    && v.get("stamp")
                        .and_then(|x| x.get("device_id"))
                        .and_then(|x| x.as_str())
                        .is_some()
                    && v.get("stamp")
                        .and_then(|x| x.get("counter"))
                        .and_then(|x| x.as_u64())
                        .is_some()
                    && v.get("text")
                        .and_then(|x| x.as_str())
                        .map(|t| {
                            hex::encode(sha2::Sha256::digest(t.as_bytes()))
                                == v.get("hash").and_then(|x| x.as_str()).unwrap_or("")
                        })
                        .unwrap_or(false)
                    && v.get("text")
                        .and_then(|x| x.as_str())
                        .map(|t| {
                            t.len() == v.get("size").and_then(|x| x.as_u64()).unwrap_or(0) as usize
                        })
                        .unwrap_or(false);
                if !valid {
                    req.respond(tiny_http::Response::empty(400))?;
                    continue;
                }
                #[cfg(feature = "wayland")]
                if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    let _ = crate::clipboard::wayland::copy_text(text);
                }
                clipboard = Some(v.clone());
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
