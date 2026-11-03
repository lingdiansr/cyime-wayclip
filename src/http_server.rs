use crate::protocol::valid_basic;
use crate::{
    protocol::{GetResult, PutResult},
    server::Server,
};
use std::io::Read;
pub fn serve_v1(addr: &str, mut state: Server<()>, pairing_code: &str) -> std::io::Result<()> {
    let server = tiny_http::Server::http(addr).map_err(std::io::Error::other)?;
    for mut req in server.incoming_requests() {
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
            certificate: identity.cert_der.clone(),
            private_key: identity.key_der.clone(),
        },
    )
    .map_err(std::io::Error::other)?;
    for mut req in server.incoming_requests() {
        let path = req.url().to_string();
        if path == "/api/pair" {
            let mut b = Vec::new();
            req.as_reader().read_to_end(&mut b)?;
            let secret = String::from_utf8_lossy(&b);
            let r = pairing.pair(&secret, "android", 0);
            let (code, body) = match r {
                Ok(x) => (201, serde_json::to_vec(&x).unwrap()),
                Err(_) => (401, vec![]),
            };
            req.respond(tiny_http::Response::from_data(body).with_status_code(code))?;
        } else {
            req.respond(tiny_http::Response::empty(404))?;
        }
    }
    Ok(())
}
