use crate::protocol::valid_basic;
use crate::{
    protocol::{GetResult, PutResult},
    server::Server,
};
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
        respond(req, r)?;
    }
    Ok(())
}

pub fn serve_v2_tls(
    addr: &str,
    state: Server<()>,
    identity: &crate::tls::Identity,
    pairing: &mut crate::protocol::PairingService,
) -> std::io::Result<()> {
    let directory = crate::config::state_dir()?;
    crate::config::ensure_private_directory(&directory)?;
    let devices =
        crate::protocol::devices::DeviceRegistry::open(Some(directory.join("devices.json")))?;
    let peers = crate::peers::PeerManager::open(Some(directory.join("peers.json")))?;
    #[cfg(feature = "wayland")]
    let (read, write, wait) = {
        use crate::clipboard::wayland::WaylandClipboard;
        match WaylandClipboard::start(wayland_seat()) {
            Ok(clipboard) => {
                let reader = clipboard.clone();
                let writer = clipboard.clone();
                let monitor = clipboard;
                let mut revision = 0u64;
                let read: crate::sync_runtime::ClipboardRead = Box::new(move || reader.read());
                let write: crate::sync_runtime::ClipboardWrite =
                    Box::new(move |text, expected_generation| {
                        writer.write(text, expected_generation)
                    });
                let wait: crate::sync_runtime::ClipboardEventWait = Box::new(move |timeout| {
                    let observed = monitor.wait_for_change(revision, timeout);
                    let changed = observed != revision;
                    revision = observed;
                    changed
                });
                (read, write, Some(wait))
            }
            Err(error) => {
                eprintln!("Wayland clipboard unavailable: {error}");
                let (read, write) = unavailable_clipboard("Wayland clipboard unavailable");
                (read, write, None)
            }
        }
    };
    #[cfg(not(feature = "wayland"))]
    let (read, write, wait): (
        crate::sync_runtime::ClipboardRead,
        crate::sync_runtime::ClipboardWrite,
        Option<crate::sync_runtime::ClipboardEventWait>,
    ) = {
        let (read, write) = unavailable_clipboard("Wayland support is disabled");
        (read, write, None)
    };
    let runtime = crate::sync_runtime::SyncRuntime::persistent(
        state.config.clone(),
        crate::config::Config::path(),
        peers,
        read,
        write,
        directory.join("clock.json"),
    )?;
    let nearby_dir = directory.join("nearby");
    let nearby = crate::discovery::Nearby::open(Some(nearby_dir), identity.fingerprint.clone())?;
    let port = addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(9876);
    runtime.configure_host(port, identity.fingerprint.clone());
    let bound: std::net::SocketAddr = addr.parse().map_err(std::io::Error::other)?;
    let addresses = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|a| a.ip())
        .filter(|ip| crate::peers::is_lan(*ip) && !ip.is_loopback())
        .filter(|ip| {
            if bound.ip().is_unspecified() {
                ip.is_ipv4() == bound.is_ipv4()
            } else {
                *ip == bound.ip()
            }
        })
        .collect::<Vec<_>>();
    let _discovery = if state.config.nearby_enabled {
        match crate::discovery::Discovery::advertise(
            &state.config.server_id,
            "CyIME Wayclip",
            &identity.fingerprint,
            port,
            &addresses,
        ) {
            Ok(service) => Some(service),
            Err(e) => {
                eprintln!("nearby discovery unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    vlog(1, format!("starting HTTPS listener on {addr}"));
    let server = tiny_http::Server::https(
        addr,
        tiny_http::SslConfig {
            certificate: identity.cert_pem.clone(),
            private_key: identity.key_pem.clone(),
        },
    )
    .map_err(std::io::Error::other)?;
    runtime.start_monitor(Some(directory.join("devices.json")), wait);
    serve_requests(
        server.incoming_requests(),
        pairing,
        runtime,
        devices,
        Some(directory.join("devices.json")),
        nearby,
        cfg!(feature = "wayland"),
    )
}
#[cfg(feature = "wayland")]
fn wayland_seat() -> Option<String> {
    std::env::var("CYIME_WAYCLIP_SEAT")
        .ok()
        .filter(|seat| !seat.trim().is_empty())
}

/// Keep the HTTP service alive with explicit 503s when no native backend exists.
type BackendClosures = (
    crate::sync_runtime::ClipboardRead,
    crate::sync_runtime::ClipboardWrite,
);
fn unavailable_clipboard(message: &'static str) -> BackendClosures {
    let read: crate::sync_runtime::ClipboardRead = Box::new(move || Err(message.into()));
    let write: crate::sync_runtime::ClipboardWrite = Box::new(move |_, _| Err(message.into()));
    (read, write)
}

// A client disconnect is local to one response, never a reason to stop serving.
fn respond<R: Read>(
    req: tiny_http::Request,
    response: tiny_http::Response<R>,
) -> std::io::Result<()> {
    if let Err(error) = req.respond(response) {
        eprintln!("HTTP response write failed: {error}");
    }
    Ok(())
}

fn json_response(
    req: tiny_http::Request,
    code: u16,
    value: serde_json::Value,
) -> std::io::Result<()> {
    let mut response = tiny_http::Response::from_string(value.to_string()).with_status_code(code);
    response.add_header(
        tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap(),
    );
    respond(req, response)
}
fn failure(
    req: tiny_http::Request,
    status: u16,
    config: &crate::config::Config,
) -> std::io::Result<()> {
    if status == 403 {
        json_response(
            req,
            status,
            serde_json::json!({"v":2,"sync_paused":true,"send_enabled":config.send_enabled,"receive_enabled":config.receive_enabled}),
        )
    } else {
        respond(req, tiny_http::Response::empty(status))
    }
}
fn bearer(req: &tiny_http::Request) -> Option<&str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .map(str::trim)
}
fn body(req: &mut tiny_http::Request) -> Result<Vec<u8>, u16> {
    let mut bytes = Vec::new();
    req.as_reader()
        .take((crate::protocol::MAX_JSON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| 400u16)?;
    if bytes.len() > crate::protocol::MAX_JSON_BYTES {
        Err(413)
    } else {
        Ok(bytes)
    }
}
fn json_body(req: &mut tiny_http::Request) -> Result<serde_json::Value, u16> {
    serde_json::from_slice(&body(req)?).map_err(|_| 400)
}
fn authenticated_body(
    req: &mut tiny_http::Request,
    path: &Option<std::path::PathBuf>,
    devices: &crate::protocol::devices::DeviceRegistry,
) -> Result<Vec<u8>, u16> {
    let bytes = body(req)?;
    let current = if path.is_some() {
        Some(crate::protocol::devices::DeviceRegistry::open(path.clone()).map_err(|_| 503u16)?)
    } else {
        None
    };
    let registry = current.as_ref().unwrap_or(devices);
    if bearer(req).and_then(|t| registry.authenticate(t)).is_none() {
        return Err(401);
    }
    Ok(bytes)
}
fn authenticated_json(
    req: &mut tiny_http::Request,
    path: &Option<std::path::PathBuf>,
    devices: &crate::protocol::devices::DeviceRegistry,
) -> Result<serde_json::Value, u16> {
    serde_json::from_slice(&authenticated_body(req, path, devices)?).map_err(|_| 400)
}
fn client_fields(v: &serde_json::Value) -> Option<(String, String)> {
    if v["v"] != 2 {
        return None;
    }
    let id = uuid::Uuid::parse_str(v["client_id"].as_str()?)
        .ok()?
        .to_string();
    let name = v["client_name"]
        .as_str()
        .or_else(|| v["name"].as_str())
        .unwrap_or("CyIME device");
    let name = name.chars().filter(|c| !c.is_control()).take(64).collect();
    Some((id, name))
}

#[allow(clippy::too_many_arguments)]
fn serve_requests(
    requests: impl Iterator<Item = tiny_http::Request>,
    pairing: &mut crate::protocol::PairingService,
    runtime: std::sync::Arc<crate::sync_runtime::SyncRuntime>,
    mut devices: crate::protocol::devices::DeviceRegistry,
    device_path: Option<std::path::PathBuf>,
    mut nearby: crate::discovery::Nearby,
    push_supported: bool,
) -> std::io::Result<()> {
    use serde_json::json;
    for mut req in requests {
        let path = req.url().to_owned();
        let method = req.method().clone();
        let config = runtime.settings();
        vlog(2, format!("request method={method:?} path={path}"));
        // CLI revocation must take effect without restarting the listener.
        if device_path.is_some() {
            match crate::protocol::devices::DeviceRegistry::open(device_path.clone()) {
                Ok(updated) => {
                    let owners = updated
                        .list()
                        .into_iter()
                        .map(|d| d.device_id)
                        .collect::<Vec<_>>();
                    let _ = runtime.peers.retain_owners(&owners);
                    devices = updated;
                }
                Err(e) => {
                    eprintln!("credential store unavailable: {e}");
                    failure(req, 503, &config)?;
                    continue;
                }
            }
        }
        if path == "/api/pair" {
            if method != tiny_http::Method::Post {
                failure(req, 405, &config)?;
                continue;
            }
            let bytes = match body(&mut req) {
                Ok(b) => b,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    failure(req, 400, &config)?;
                    continue;
                }
            };
            let (client_id, name) = match client_fields(&value) {
                Some(fields) => fields,
                None => {
                    // Keep the original QR client's secret-only request compatible.
                    if value.get("v").is_some() {
                        failure(req, 400, &config)?;
                        continue;
                    }
                    (uuid::Uuid::new_v4().to_string(), "CyIME device".into())
                }
            };
            let secret = value["secret"].as_str().unwrap_or("");
            let now = crate::sync_runtime::now_ms() as u64 / 1000;
            if pairing.check_offer(secret, now).is_err() {
                failure(req, 401, &config)?;
                continue;
            }
            match devices.issue(&client_id, &name) {
                Ok((device_id, access_token)) => {
                    let _ = runtime.peers.remove_owner(&device_id);
                    pairing.consume_offer();
                    json_response(
                        req,
                        201,
                        json!({"v":2,"server_id":config.server_id,"server_name":"CyIME Wayclip","device_id":device_id,"access_token":access_token}),
                    )?;
                }
                Err(e) => {
                    eprintln!("persist pairing failed: {e}");
                    failure(req, 503, &config)?;
                }
            }
            continue;
        }
        if path == "/api/pair/nearby" {
            if method != tiny_http::Method::Post {
                failure(req, 405, &config)?;
                continue;
            }
            if !config.nearby_enabled {
                failure(req, 403, &config)?;
                continue;
            }
            let value = match json_body(&mut req) {
                Ok(v) => v,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            let remote = match req.remote_addr() {
                Some(a) => a.ip(),
                None => {
                    failure(req, 400, &config)?;
                    continue;
                }
            };
            match nearby.request(&value, remote, crate::sync_runtime::now_ms() as u64) {
                Ok(crate::discovery::NearbyResult::Pending {
                    request_id,
                    code,
                    name,
                }) => {
                    eprintln!("nearby pairing pending id={request_id} name={name:?} code={code}; approve locally after comparing codes");
                    json_response(req, 202, json!({"v":2,"pending":true}))?;
                }
                Ok(crate::discovery::NearbyResult::Approved {
                    request_id,
                    client_id,
                    name,
                }) => match devices.issue(&client_id, &name) {
                    Ok((device_id, access_token)) => {
                        let _ = runtime.peers.remove_owner(&device_id);
                        if let Err(e) = nearby.complete(&request_id) {
                            eprintln!("nearby completion failed: {e}");
                            failure(req, 503, &config)?;
                            continue;
                        }
                        json_response(
                            req,
                            201,
                            json!({"v":2,"server_id":config.server_id,"server_name":"CyIME Wayclip","device_id":device_id,"access_token":access_token}),
                        )?;
                    }
                    Err(e) => {
                        eprintln!("persist nearby pairing failed: {e}");
                        failure(req, 503, &config)?;
                    }
                },
                Err(_) => failure(req, 403, &config)?,
            }
            continue;
        }
        let owner = match bearer(&req).and_then(|token| devices.authenticate(token)) {
            Some(id) => id,
            None => {
                failure(req, 401, &config)?;
                continue;
            }
        };
        if path == "/api/status" {
            if method != tiny_http::Method::Get {
                failure(req, 405, &config)?;
                continue;
            }
            let back = runtime.peers.back(&owner);
            let pin = back.as_ref().map(|p| p.fingerprint.clone());
            let port = back.as_ref().and_then(|p| {
                p.origin
                    .rsplit_once(':')
                    .and_then(|(_, p)| p.parse::<u16>().ok())
            });
            json_response(
                req,
                200,
                json!({"v":2,"connected":true,"device_id":owner,"send_enabled":config.send_enabled,"receive_enabled":config.receive_enabled,"push":push_supported && config.send_enabled,"push_pin":pin,"push_port":port,"push_failed":runtime.peers.failed(&owner)}),
            )?;
        } else if path == "/api/clipboard" && method == tiny_http::Method::Get {
            match runtime.get() {
                Ok(Some(value)) => {
                    let hash = value["hash"].as_str().unwrap();
                    let matches = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("If-None-Match"))
                        .is_some_and(|h| {
                            h.value
                                .as_str()
                                .split(',')
                                .any(|v| v.trim().trim_matches('"') == hash)
                        });
                    let mut response = if matches {
                        tiny_http::Response::empty(304).boxed()
                    } else {
                        tiny_http::Response::from_string(value.to_string())
                            .with_status_code(200)
                            .boxed()
                    };
                    response.add_header(
                        tiny_http::Header::from_bytes("ETag", format!("\"{hash}\"")).unwrap(),
                    );
                    response.add_header(
                        tiny_http::Header::from_bytes(
                            "Content-Type",
                            "application/json; charset=utf-8",
                        )
                        .unwrap(),
                    );
                    respond(req, response)?;
                }
                Ok(None) => {
                    respond(req, tiny_http::Response::empty(204))?;
                }
                Err(e) => failure(req, e.status, &config)?,
            }
        } else if path == "/api/clipboard" && method == tiny_http::Method::Put {
            let bytes = match authenticated_body(&mut req, &device_path, &devices) {
                Ok(b) => b,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            match runtime.put(&bytes, &owner) {
                Ok(value) => {
                    let mut response = tiny_http::Response::from_string(value.to_string());
                    response.add_header(
                        tiny_http::Header::from_bytes(
                            "Content-Type",
                            "application/json; charset=utf-8",
                        )
                        .unwrap(),
                    );
                    response.add_header(
                        tiny_http::Header::from_bytes(
                            "ETag",
                            format!("\"{}\"", value["hash"].as_str().unwrap()),
                        )
                        .unwrap(),
                    );
                    respond(req, response)?;
                }
                Err(e) => {
                    eprintln!(
                        "clipboard PUT rejected status={} reason={}",
                        e.status, e.reason
                    );
                    failure(req, e.status, &config)?;
                }
            }
        } else if path == "/api/peer" && method == tiny_http::Method::Post {
            let value = match authenticated_json(&mut req, &device_path, &devices) {
                Ok(v) => v,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            let remote = match req.remote_addr() {
                Some(a) => a.ip(),
                None => {
                    failure(req, 400, &config)?;
                    continue;
                }
            };
            match crate::peers::Peer::registration(&value, remote) {
                Ok(peer) => {
                    if peer.id == config.server_id {
                        failure(req, 400, &config)?;
                        continue;
                    }
                    match runtime.peers.register(&owner, peer) {
                        Ok(()) => {
                            runtime.mesh.trigger();
                            if let Ok(Some(v)) = runtime.get() {
                                runtime.peers.publish(v);
                            }
                            json_response(
                                req,
                                200,
                                json!({"v":2,"push":push_supported && config.send_enabled}),
                            )?;
                        }
                        Err(_) => failure(req, 409, &config)?,
                    }
                }
                Err(_) => failure(req, 400, &config)?,
            }
        } else if path == "/api/grant" && method == tiny_http::Method::Post {
            let value = match authenticated_json(&mut req, &device_path, &devices) {
                Ok(v) => v,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            if !config.allow_introductions {
                failure(req, 403, &config)?;
                continue;
            }
            let (client, name) = match client_fields(&value) {
                Some(v) => v,
                None => {
                    failure(req, 400, &config)?;
                    continue;
                }
            };
            if client == config.server_id {
                failure(req, 400, &config)?;
                continue;
            }
            match devices.grant(bearer(&req).unwrap(), &client, &name) {
                Ok((_, token)) => json_response(req, 200, json!({"v":2,"token":token}))?,
                Err(_) => failure(req, 409, &config)?,
            }
        } else if path == "/api/introduce" && method == tiny_http::Method::Post {
            let value = match authenticated_json(&mut req, &device_path, &devices) {
                Ok(v) => v,
                Err(code) => {
                    failure(req, code, &config)?;
                    continue;
                }
            };
            if !config.allow_introductions {
                failure(req, 403, &config)?;
                continue;
            }
            match crate::peers::Peer::introduction(&value) {
                Ok(peer) => {
                    if peer.id == config.server_id {
                        failure(req, 400, &config)?;
                        continue;
                    }
                    // A grant is scoped to its introducer, never a rotation of a direct pairing.
                    let (peer_owner, token) =
                        match devices.grant(bearer(&req).unwrap(), &peer.id, &peer.name) {
                            Ok(g) => g,
                            Err(e) => {
                                eprintln!("persist introduced grant failed: {e}");
                                failure(req, 503, &config)?;
                                continue;
                            }
                        };
                    let registration = match runtime.registration(token) {
                        Some(v) => v,
                        None => {
                            failure(req, 503, &config)?;
                            continue;
                        }
                    };
                    match runtime.peers.register(&peer_owner, peer) {
                        Ok(()) => {
                            // Do not block this listener: two introducing peers can register
                            // callbacks with each other at exactly the same time.
                            match runtime.peers.register_back(&peer_owner, registration) {
                                Ok(()) => {
                                    if let Ok(Some(v)) = runtime.get() {
                                        runtime.peers.publish(v);
                                    }
                                    json_response(req, 200, json!({"v":2}))?;
                                }
                                Err(_) => failure(req, 503, &config)?,
                            }
                        }
                        Err(_) => failure(req, 409, &config)?,
                    }
                }
                Err(_) => failure(req, 400, &config)?,
            }
        } else {
            failure(
                req,
                if path == "/api/clipboard" { 405 } else { 404 },
                &config,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "http_server_tests.rs"]
mod tests;
