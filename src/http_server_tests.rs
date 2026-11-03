//! Route-level integration tests: real HTTP framing on ephemeral loopback sockets.
use super::serve_requests;
use crate::{
    config::Config,
    discovery::Nearby,
    peers::PeerManager,
    protocol::{
        devices::DeviceRegistry, encode_secret, ClipboardProfile, PairOffer, PairingService,
    },
    sync_runtime::{now_ms, SyncRuntime},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

struct PrivateState(PathBuf);
impl PrivateState {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cyime-http-tests-{}", uuid::Uuid::new_v4()));
        crate::config::ensure_private_directory(&path).unwrap();
        Self(path)
    }
}
impl Drop for PrivateState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct MemoryClipboard {
    text: Option<String>,
    read_error: bool,
    write_error: bool,
    writes: usize,
}
struct Listener {
    server: Arc<tiny_http::Server>,
    join: Option<JoinHandle<std::io::Result<()>>>,
    addr: SocketAddr,
    token: String,
    owner: String,
    secret: String,
    clipboard: Arc<Mutex<MemoryClipboard>>,
    runtime: Arc<SyncRuntime>,
}
impl Listener {
    fn start(root: &Path, config: Config, requests: usize, text: Option<&str>) -> Self {
        let device_path = root.join("devices.json");
        let mut devices = DeviceRegistry::open(Some(device_path.clone())).unwrap();
        let (owner, token) = devices
            .issue(&uuid::Uuid::new_v4().to_string(), "test owner")
            .unwrap();
        let peers = PeerManager::open(None).unwrap();
        let clipboard = Arc::new(Mutex::new(MemoryClipboard {
            text: None,
            ..MemoryClipboard::default()
        }));
        let read_state = clipboard.clone();
        let write_state = clipboard.clone();
        let runtime = SyncRuntime::new(
            config.clone(),
            None,
            peers,
            Box::new(move || {
                let state = read_state.lock().unwrap();
                if state.read_error {
                    Err("injected read failure".into())
                } else {
                    Ok(state.text.clone())
                }
            }),
            Box::new(move |text| {
                let mut state = write_state.lock().unwrap();
                if state.write_error {
                    return Err("injected write failure".into());
                }
                state.text = Some(text.to_owned());
                state.writes += 1;
                Ok(())
            }),
        );
        // Model a copy after the service has observed its initial empty baseline.
        clipboard.lock().unwrap().text = text.map(str::to_owned);
        let mut pairing = PairingService::new(config.server_id.clone(), "test host");
        let secret = encode_secret(&[11; 32]).unwrap();
        pairing.set_offer(PairOffer {
            v: 2,
            server_id: config.server_id,
            name: "test host".into(),
            certificate_sha256: "ab".repeat(32),
            pairing_id: uuid::Uuid::new_v4().to_string(),
            secret: secret.clone(),
            endpoints: vec!["https://127.0.0.1:1".into()],
            expires_at: now_ms() as u64 / 1000 + 120,
        });
        let nearby = Nearby::open(Some(root.join("nearby")), "ab".repeat(32)).unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let addr = server.server_addr().to_ip().unwrap();
        let serving = server.clone();
        let shared = runtime.clone();
        let join = thread::spawn(move || {
            serve_requests(
                serving.incoming_requests().take(requests),
                &mut pairing,
                shared,
                devices,
                Some(device_path),
                nearby,
                true,
            )
        });
        Self {
            server,
            join: Some(join),
            addr,
            token,
            owner,
            secret,
            clipboard,
            runtime,
        }
    }
    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> HttpResponse {
        let mut socket = TcpStream::connect_timeout(&self.addr, Duration::from_secs(3)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(socket, "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", self.addr, body.len()).unwrap();
        if let Some(token) = token {
            write!(socket, "Authorization: Bearer {token}\r\n").unwrap();
        }
        for (name, value) in headers {
            write!(socket, "{name}: {value}\r\n").unwrap();
        }
        socket.write_all(b"\r\n").unwrap();
        socket.write_all(body).unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        let split = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP response headers");
        let head = std::str::from_utf8(&bytes[..split]).unwrap();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = head
            .lines()
            .skip(1)
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        HttpResponse {
            status,
            headers,
            body: bytes[split + 4..].to_vec(),
        }
    }
    fn json(&self, method: &str, path: &str, value: Value) -> HttpResponse {
        self.send(
            method,
            path,
            Some(&self.token),
            &serde_json::to_vec(&value).unwrap(),
            &[],
        )
    }
    fn get(&self, path: &str) -> HttpResponse {
        self.send("GET", path, Some(&self.token), b"", &[])
    }
    fn finish(&mut self) {
        self.join
            .take()
            .unwrap()
            .join()
            .expect("listener panicked")
            .expect("listener returned an I/O error");
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        // Also unblock early test failures so a panic cannot leave a hanging listener.
        self.server.unblock();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
impl HttpResponse {
    fn value(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON HTTP body")
    }
}
fn profile(text: &str, stamp: i64) -> Value {
    let mut value = serde_json::to_value(ClipboardProfile::text(text).unwrap()).unwrap();
    value["stamp"] = json!(stamp);
    value
}
fn peer_value() -> Value {
    json!({"v":2,"id":uuid::Uuid::new_v4().to_string(),"name":"callback device", "port":18443,
        "origin":"https://8.8.8.8:443", "fingerprint":"ab".repeat(32),"token":encode_secret(&[12;32]).unwrap()})
}

#[test]
fn qr_pairing_secret_consumption_and_persistent_token_restart() {
    let state = PrivateState::new();
    let config = Config::default();
    let mut listener = Listener::start(&state.0, config.clone(), 4, None);
    let client = uuid::Uuid::new_v4().to_string();
    let mut request =
        json!({"v":2,"client_id":client,"client_name":"paired phone","secret":"wrong"});
    assert_eq!(
        listener.json("POST", "/api/pair", request.clone()).status,
        401
    );
    request["secret"] = listener.secret.clone().into();
    let paired = listener.json("POST", "/api/pair", request.clone());
    assert_eq!(paired.status, 201);
    let paired = paired.value();
    assert_eq!(paired["server_id"], config.server_id);
    let token = paired["access_token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 43);
    assert_eq!(
        listener.json("POST", "/api/pair", request).status,
        401,
        "QR offer must be one-shot"
    );
    assert_eq!(
        listener
            .send("GET", "/api/status", Some(&token), b"", &[])
            .status,
        200
    );
    listener.finish();
    drop(listener);
    let stored = fs::read_to_string(state.0.join("devices.json")).unwrap();
    assert!(
        !stored.contains(&token),
        "credential store must contain a digest, not token plaintext"
    );
    let registry = DeviceRegistry::open(Some(state.0.join("devices.json"))).unwrap();
    assert_eq!(
        registry.authenticate(&token),
        paired["device_id"].as_str().map(str::to_owned)
    );
    let mut restarted = Listener::start(&state.0, config, 1, None);
    let status = restarted.send("GET", "/api/status", Some(&token), b"", &[]);
    assert_eq!(status.status, 200);
    assert_eq!(status.value()["device_id"], paired["device_id"]);
    restarted.finish();
}

#[test]
fn malformed_and_oversized_bodies_do_not_stop_listener() {
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 5, Some("unchanged"));
    assert_eq!(
        listener
            .send("PUT", "/api/clipboard", Some(&listener.token), b"{", &[])
            .status,
        400
    );
    let too_large = vec![b'x'; crate::protocol::MAX_JSON_BYTES + 1];
    assert_eq!(
        listener
            .send(
                "PUT",
                "/api/clipboard",
                Some(&listener.token),
                &too_large,
                &[]
            )
            .status,
        413
    );
    assert_eq!(
        listener
            .send("POST", "/api/pair", None, b"not JSON", &[])
            .status,
        400
    );
    assert_eq!(
        listener
            .send("POST", "/api/pair", None, &too_large, &[])
            .status,
        413
    );
    let good = listener.get("/api/clipboard");
    assert_eq!(good.status, 200);
    assert_eq!(good.value()["text"], "unchanged");
    listener.finish();
}

#[test]
fn clipboard_stamps_etags_and_duplicate_put_are_stable() {
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 7, Some("desktop copy"));
    let first = listener.get("/api/clipboard");
    assert_eq!(first.status, 200);
    let etag = first.headers["etag"].clone();
    let first_value = first.value();
    assert!(first_value["stamp"].as_i64().unwrap() > 0);
    let second = listener.get("/api/clipboard");
    assert_eq!(second.value(), first_value);
    assert_eq!(second.headers["etag"], etag);
    let not_modified = listener.send(
        "GET",
        "/api/clipboard",
        Some(&listener.token),
        b"",
        &[("If-None-Match", &etag)],
    );
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());
    let incoming = profile("phone copy", now_ms() + 60_000);
    let put = listener.json("PUT", "/api/clipboard", incoming.clone());
    assert_eq!(put.status, 200);
    assert_eq!(put.value()["stamp"], incoming["stamp"]);
    let updated = listener.get("/api/clipboard");
    assert_eq!(updated.value()["text"], "phone copy");
    assert_eq!(updated.value()["stamp"], incoming["stamp"]);
    assert_ne!(updated.headers["etag"], etag);
    assert_eq!(listener.json("PUT", "/api/clipboard", incoming).status, 200);
    assert_eq!(
        listener.get("/api/clipboard").value()["stamp"],
        updated.value()["stamp"]
    );
    assert_eq!(
        listener.clipboard.lock().unwrap().writes,
        1,
        "duplicate PUT must not rewrite desktop"
    );
    listener.finish();
}

#[test]
fn desktop_read_and_write_failures_return_503_not_false_success() {
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 4, Some("desktop"));
    listener.clipboard.lock().unwrap().read_error = true;
    assert_eq!(listener.get("/api/clipboard").status, 503);
    assert_eq!(
        listener
            .json("PUT", "/api/clipboard", profile("phone", now_ms() + 60_000))
            .status,
        503
    );
    {
        let mut memory = listener.clipboard.lock().unwrap();
        memory.read_error = false;
        memory.write_error = true;
    }
    assert_eq!(listener.get("/api/clipboard").status, 200);
    assert_eq!(
        listener
            .json("PUT", "/api/clipboard", profile("phone", now_ms() + 60_000))
            .status,
        503
    );
    assert_eq!(listener.clipboard.lock().unwrap().writes, 0);
    listener.finish();
}

#[test]
fn status_and_paused_directions_match_actual_access() {
    for send_disabled in [true, false] {
        let state = PrivateState::new();
        let config = Config {
            send_enabled: !send_disabled,
            receive_enabled: send_disabled,
            ..Config::default()
        };
        let mut listener = Listener::start(&state.0, config.clone(), 3, Some("desktop"));
        let status = listener.get("/api/status");
        assert_eq!(status.status, 200);
        let value = status.value();
        assert_eq!(value["send_enabled"], config.send_enabled);
        assert_eq!(value["receive_enabled"], config.receive_enabled);
        assert_eq!(value["push"], config.send_enabled);
        let get = listener.get("/api/clipboard");
        let put = listener.json("PUT", "/api/clipboard", profile("phone", now_ms() + 60_000));
        let paused = if send_disabled {
            assert_eq!(put.status, 200);
            get
        } else {
            assert_eq!(get.status, 200);
            put
        };
        assert_eq!(paused.status, 403);
        assert_eq!(paused.value()["sync_paused"], true);
        assert_eq!(paused.value()["send_enabled"], config.send_enabled);
        assert_eq!(paused.value()["receive_enabled"], config.receive_enabled);
        listener.finish();
    }
}

#[test]
fn mesh_endpoints_require_bearer_and_introduction_is_opt_in() {
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 5, None);
    for path in ["/api/peer", "/api/introduce", "/api/grant"] {
        assert_eq!(
            listener.send("POST", path, None, b"{}", &[]).status,
            401,
            "{path}"
        );
    }
    assert_eq!(
        listener.json("POST", "/api/introduce", peer_value()).status,
        403
    );
    assert_eq!(listener.json("POST", "/api/grant", json!({"v":2,"client_id":uuid::Uuid::new_v4().to_string(),"client_name":"third phone"})).status, 403);
    listener.finish();
}

#[test]
fn registration_uses_request_source_and_public_introductions_are_rejected() {
    let state = PrivateState::new();
    let config = Config {
        allow_introductions: true,
        ..Config::default()
    };
    let mut listener = Listener::start(&state.0, config, 4, None);
    let request = peer_value();
    let registered = listener.json("POST", "/api/peer", request.clone());
    assert_eq!(registered.status, 200);
    let peer = listener.runtime.peers.back(&listener.owner).unwrap();
    assert_eq!(
        peer.origin, "https://127.0.0.1:18443",
        "body origin is untrusted; use real TCP source"
    );
    assert_eq!(peer.id, request["id"]);
    assert_eq!(
        listener
            .json("POST", "/api/introduce", request.clone())
            .status,
        400,
        "public literal address is not a LAN peer"
    );
    let mut invalid_port = request.clone();
    invalid_port["port"] = 0.into();
    assert_eq!(listener.json("POST", "/api/peer", invalid_port).status, 400);
    let mut invalid_pin = request.clone();
    invalid_pin["fingerprint"] = "bad".into();
    assert_eq!(listener.json("POST", "/api/peer", invalid_pin).status, 400);
    assert!(crate::peers::Peer::registration(&request, "8.8.8.8".parse().unwrap()).is_err());
    listener.finish();
}

#[test]
fn nearby_is_pending_until_local_decision_and_cannot_reissue_token() {
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 5, None);
    let request = json!({"v":2,"client_id":uuid::Uuid::new_v4().to_string(),"client_name":"nearby phone","proof":encode_secret(&[13;32]).unwrap()});
    for _ in 0..2 {
        let pending = listener.send(
            "POST",
            "/api/pair/nearby",
            None,
            &serde_json::to_vec(&request).unwrap(),
            &[],
        );
        assert_eq!(pending.status, 202);
        assert_eq!(pending.value()["pending"], true);
        assert!(pending.value().get("access_token").is_none());
    }
    assert_eq!(
        listener.send("GET", "/api/status", None, b"", &[]).status,
        401
    );
    let pending = crate::discovery::pending(&state.0.join("nearby")).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].code.len(), 8);
    crate::discovery::decide(&state.0.join("nearby"), &pending[0].request_id, true).unwrap();
    let approved = listener.send(
        "POST",
        "/api/pair/nearby",
        None,
        &serde_json::to_vec(&request).unwrap(),
        &[],
    );
    assert_eq!(approved.status, 201);
    let token = approved.value()["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(token.len(), 43);
    assert!(DeviceRegistry::open(Some(state.0.join("devices.json")))
        .unwrap()
        .authenticate(&token)
        .is_some());
    assert_eq!(
        listener
            .send(
                "POST",
                "/api/pair/nearby",
                None,
                &serde_json::to_vec(&request).unwrap(),
                &[]
            )
            .status,
        403
    );
    listener.finish();
}

#[cfg(unix)]
#[test]
fn disconnect_during_request_body_does_not_stop_the_listener() {
    use std::os::fd::AsRawFd;
    let state = PrivateState::new();
    let mut listener = Listener::start(&state.0, Config::default(), 2, None);
    let mut aborted = TcpStream::connect(listener.addr).unwrap();
    write!(aborted,"POST /api/peer HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 262146\r\n\r\n{{",listener.token).unwrap();
    aborted.flush().unwrap();
    thread::sleep(Duration::from_millis(10));
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: the socket fd is live and the pointer/length describe a linger value.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                aborted.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of_val(&linger) as libc::socklen_t,
            )
        },
        0
    );
    drop(aborted);
    assert_eq!(
        listener
            .send("GET", "/api/status", Some(&listener.token), b"", &[])
            .status,
        200
    );
    listener.server.unblock();
    listener.finish();
}
