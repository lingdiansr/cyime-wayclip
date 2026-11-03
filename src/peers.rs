//! Authenticated outbound peers. Pins are certificate identities, not CA trust bypasses:
//! every handshake must prove possession of the pinned certificate's private key.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

const LIMIT: usize = 262_144;
const WORKERS: usize = 4;
#[derive(Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub origin: String,
    pub fingerprint: String,
    pub token: String,
}
impl fmt::Debug for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Peer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("fingerprint", &self.fingerprint)
            .field("token", &"[REDACTED]")
            .finish()
    }
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}
pub(crate) fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private() || a.is_link_local() || a.is_loopback(),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_lan(IpAddr::V4(v4));
            }
            a.is_loopback()
                || a.segments()[0] & 0xfe00 == 0xfc00
                || a.segments()[0] & 0xffc0 == 0xfe80
        }
    }
}
// Strict authority-only grammar: no DNS, userinfo, escapes, paths, query or fragment.
fn endpoint(origin: &str) -> Result<SocketAddr, String> {
    let authority = origin.strip_prefix("https://").ok_or("HTTPS required")?;
    let addr: SocketAddr = authority
        .parse()
        .map_err(|_| "literal IP and port required")?;
    if addr.port() == 0 || !is_lan(addr.ip()) {
        return Err("LAN address required".into());
    }
    Ok(addr)
}
impl Peer {
    pub fn registration(value: &Value, remote: IpAddr) -> Result<Self, String> {
        let port = value
            .get("port")
            .and_then(Value::as_u64)
            .filter(|p| (1..=65535).contains(p))
            .ok_or("invalid port")?;
        Self::parse(
            value,
            format!("https://{}", SocketAddr::new(remote, port as u16)),
        )
    }
    pub fn introduction(value: &Value) -> Result<Self, String> {
        Self::parse(value, field(value, "origin")?.into())
    }
    fn parse(value: &Value, origin: String) -> Result<Self, String> {
        if value.get("v").and_then(Value::as_u64) != Some(2) {
            return Err("v must be 2".into());
        }
        endpoint(&origin)?;
        let id = uuid::Uuid::parse_str(field(value, "id")?)
            .map_err(|_| "invalid UUID")?
            .to_string();
        let fingerprint = field(value, "fingerprint")?;
        if fingerprint.len() != 64
            || !fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid fingerprint".into());
        }
        let token = field(value, "token")?;
        if token.len() != 43
            || URL_SAFE_NO_PAD
                .decode(token)
                .map(|b| b.len() != 32)
                .unwrap_or(true)
        {
            return Err("invalid token".into());
        }
        let name: String = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        Ok(Self {
            id,
            name: if name.trim().is_empty() {
                "CyIME 设备".into()
            } else {
                name
            },
            origin,
            fingerprint: fingerprint.into(),
            token: token.into(),
        })
    }
    fn validate(&self) -> io::Result<()> {
        Self::introduction(&serde_json::json!({"v":2,"id":self.id,"name":self.name,"origin":self.origin,"fingerprint":self.fingerprint,"token":self.token})).map(|_| ()).map_err(|_| invalid("invalid peer"))
    }
}

#[derive(Debug)]
struct Pin {
    digest: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(cert.as_ref()).as_slice() != self.digest {
            return Err(rustls::Error::General(
                "peer certificate pin mismatch".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
/// Bounds every underlying TLS I/O, including internal handshake/trickle reads.
struct DeadlineStream {
    socket: TcpStream,
    deadline: Instant,
}
impl DeadlineStream {
    fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "peer request deadline",
            ));
        }
        Ok(Duration::from_secs(3).min(remaining))
    }
}
impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        let result = self.socket.read(buf);
        self.remaining()?;
        result
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        let result = self.socket.write(buf);
        self.remaining()?;
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}
/// Real HTTPS POST; redirects are returned as statuses, never followed.
pub fn post_json(peer: &Peer, path: &str, body: &Value) -> io::Result<(u16, Option<Value>)> {
    request(peer, "POST", path, body, None)
}
fn request(
    peer: &Peer,
    method: &str,
    path: &str,
    body: &Value,
    cancel: Option<(&Shared, usize, &str, u64)>,
) -> io::Result<(u16, Option<Value>)> {
    peer.validate()?;
    if !path.starts_with("/api/")
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b))
    {
        return Err(invalid("invalid request path"));
    }
    let addr = endpoint(&peer.origin).map_err(|_| invalid("invalid endpoint"))?;
    let data = serde_json::to_vec(body)?;
    if data.len() > LIMIT {
        return Err(invalid("request too large"));
    }
    let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    let deadline = Instant::now() + Duration::from_secs(12);
    if let Some((shared, slot, owner, generation)) = cancel {
        let mut state = shared.state.lock().unwrap();
        if state.stop
            || !state.enabled
            || state
                .entries
                .get(owner)
                .is_none_or(|e| e.generation != generation)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "peer worker stopped",
            ));
        }
        state.sockets[slot] = Some((owner.to_owned(), socket.try_clone()?));
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = Arc::new(Pin {
        digest: hex::decode(&peer.fingerprint).map_err(|_| invalid("invalid pin"))?,
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid("TLS versions"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(config), ServerName::IpAddress(addr.ip().into()))
            .map_err(|_| invalid("TLS configuration"))?;
    let mut stream = rustls::StreamOwned::new(conn, DeadlineStream { socket, deadline });
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", peer.token, data.len())?;
    stream.write_all(&data)?;
    stream.flush()?;
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        stream.sock.remaining()?;
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        if response.len() + n > LIMIT {
            return Err(invalid("peer response too large"));
        }
        response.extend_from_slice(&buf[..n]);
        if let Some(end) = header_end(&response) {
            let headers = std::str::from_utf8(&response[..end])
                .map_err(|_| invalid("invalid HTTP headers"))?;
            if let Some(length) = content_length(headers)? {
                if response.len() >= end + 4 + length {
                    break;
                }
            }
        }
    }
    stream.sock.remaining()?;
    parse_response(&response)
}
fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}
fn content_length(headers: &str) -> io::Result<Option<usize>> {
    let mut length = None;
    for line in headers.lines().skip(1) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid HTTP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            let n = value
                .trim()
                .parse::<usize>()
                .map_err(|_| invalid("invalid content length"))?;
            if n > LIMIT || length.is_some() {
                return Err(invalid("invalid content length"));
            }
            length = Some(n);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("unsupported transfer encoding"));
        }
    }
    Ok(length)
}
fn parse_response(bytes: &[u8]) -> io::Result<(u16, Option<Value>)> {
    let end = header_end(bytes).ok_or_else(|| invalid("incomplete HTTP response"))?;
    let headers =
        std::str::from_utf8(&bytes[..end]).map_err(|_| invalid("invalid HTTP headers"))?;
    let mut status = headers.lines().next().unwrap_or("").split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err(invalid("invalid HTTP version"));
    }
    let code = status
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| invalid("invalid HTTP status"))?;
    let body = &bytes[end + 4..];
    if let Some(n) = content_length(headers)? {
        if body.len() != n {
            return Err(invalid("incomplete HTTP body"));
        }
    }
    // Error bodies need not be JSON (e.g. an HTTP 413 from an intermediary).
    Ok((
        code,
        if body.is_empty() {
            None
        } else {
            serde_json::from_slice(body).ok()
        },
    ))
}

fn owned_by(key: &str, owner: &str) -> bool {
    key == owner
        || key
            .strip_prefix("introduced:")
            .and_then(|rest| rest.strip_prefix(owner))
            .is_some_and(|rest| rest.starts_with(':'))
}
// Reject symlink components, including the leaf, before both reads and replacement.
fn reject_symlinks(path: &std::path::Path) -> io::Result<()> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        match fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("peer store symlink refused"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn read_store(path: &std::path::Path) -> io::Result<Vec<u8>> {
    reject_symlinks(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    // Linux O_NOFOLLOW closes the leaf check/open race. Parent directories are
    // local configuration owned by this user; reject symlink parents above too.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("peer store must be regular file"));
    }
    reject_symlinks(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let opened = file.metadata()?;
        let current = fs::symlink_metadata(path)?;
        if opened.dev() != current.dev() || opened.ino() != current.ino() {
            return Err(invalid("peer store changed while opening"));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(invalid("peer store too large"));
    }
    Ok(bytes)
}

#[derive(Clone)]
struct Pending {
    body: Value,
    sequence: u64,
    attempt: usize,
    due: Instant,
}
#[derive(Clone)]
struct Entry {
    peer: Peer,
    generation: u64,
    failed: bool,
    busy: bool,
    pending: Option<Pending>,
    control: Option<Pending>,
    control_blocked: bool,
}
struct State {
    entries: BTreeMap<String, Entry>,
    sequence: u64,
    persistence_dirty: bool,
    enabled: bool,
    stop: bool,
    sockets: Vec<Option<(String, TcpStream)>>,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}
struct Manager {
    mesh_request: Mutex<()>,
    shared: Arc<Shared>,
    path: Option<PathBuf>,
}
impl Drop for Manager {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.stop = true;
        for (_, socket) in state.sockets.iter().flatten() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.shared.wake.notify_all();
    }
}
/// Clones share four bounded workers. Last handle drop wakes retries and cancels active I/O.
#[derive(Clone)]
pub struct PeerManager {
    inner: Arc<Manager>,
}
impl PeerManager {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let saved: BTreeMap<String, Peer> = match path.as_ref().map(|p| read_store(p)) {
            Some(Ok(bytes)) => {
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid peer store"))?
            }
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => BTreeMap::new(),
        };
        if saved.len() > 32 {
            return Err(invalid("too many peers"));
        }
        let mut entries = BTreeMap::new();
        for (owner, peer) in saved {
            peer.validate()?;
            entries.insert(
                owner,
                Entry {
                    peer,
                    generation: 0,
                    failed: false,
                    busy: false,
                    pending: None,
                    control: None,
                    control_blocked: false,
                },
            );
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                entries,
                sequence: 0,
                persistence_dirty: false,
                enabled: true,
                stop: false,
                sockets: (0..=WORKERS).map(|_| None).collect(),
            }),
            wake: Condvar::new(),
        });
        let manager = Self {
            inner: Arc::new(Manager {
                mesh_request: Mutex::new(()),
                shared: shared.clone(),
                path,
            }),
        };
        for slot in 0..WORKERS {
            let shared = shared.clone();
            thread::Builder::new()
                .name(format!("peer-push-{slot}"))
                .spawn(move || worker(shared, slot))?;
        }
        Ok(manager)
    }
    /// Secret-bearing snapshots are for the authenticated mesh coordinator only.
    pub fn snapshot(&self) -> Vec<(String, Peer)> {
        self.inner
            .shared
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|(owner, entry)| (owner.clone(), entry.peer.clone()))
            .collect()
    }
    /// One coordinator may make synchronous control requests; never call this
    /// blocking method on the HTTP listener. It shares all revocation checks.
    pub fn post_owned(
        &self,
        owner: &str,
        path: &str,
        body: &Value,
    ) -> io::Result<(u16, Option<Value>)> {
        let _serial = self.inner.mesh_request.lock().unwrap();
        let shared = &self.inner.shared;
        let (generation, peer) = {
            let state = shared.state.lock().unwrap();
            if state.stop || !state.enabled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "peer sending paused",
                ));
            }
            let entry = state
                .entries
                .get(owner)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "peer owner not found"))?;
            (entry.generation, entry.peer.clone())
        };
        let result = request(
            &peer,
            "POST",
            path,
            body,
            Some((shared, WORKERS, owner, generation)),
        );
        let mut state = shared.state.lock().unwrap();
        state.sockets[WORKERS] = None;
        if state.stop
            || !state.enabled
            || state
                .entries
                .get(owner)
                .is_none_or(|e| e.generation != generation)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "peer owner revoked or paused",
            ));
        }
        result
    }
    pub fn register(&self, owner_id: &str, peer: Peer) -> io::Result<()> {
        peer.validate()?;
        if owner_id.is_empty() || owner_id.len() > 128 {
            return Err(invalid("invalid owner"));
        }
        let mut state = self.inner.shared.state.lock().unwrap();
        let identity = uuid::Uuid::parse_str(&peer.id).map_err(|_| invalid("invalid peer UUID"))?;
        if state.entries.values().any(|entry| {
            uuid::Uuid::parse_str(&entry.peer.id).ok() == Some(identity)
                && entry.peer.fingerprint != peer.fingerprint
        }) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer identity pin conflict",
            ));
        }
        if !state.entries.contains_key(owner_id) && state.entries.len() >= 32 {
            return Err(invalid("peer limit reached"));
        }
        let mut entries = state.entries.clone();
        state.sequence += 1;
        entries.insert(
            owner_id.into(),
            Entry {
                peer,
                generation: state.sequence,
                failed: false,
                busy: false,
                pending: None,
                control: None,
                control_blocked: false,
            },
        );
        self.save(&entries)?;
        state.persistence_dirty = false;
        for (owner, socket) in state.sockets.iter().flatten() {
            if owner == owner_id {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        state.entries = entries;
        self.inner.shared.wake.notify_all();
        Ok(())
    }
    /// Queue a pinned reverse registration without blocking the HTTP listener.
    /// A paused manager rejects this operation; callers must reintroduce after resume.
    pub fn register_back(&self, owner: &str, body: Value) -> io::Result<()> {
        if !body.is_object() || serde_json::to_vec(&body)?.len() > LIMIT {
            return Err(invalid("invalid reverse registration body"));
        }
        let mut state = self.inner.shared.state.lock().unwrap();
        if !state.entries.contains_key(owner) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "peer owner not found",
            ));
        }
        if !state.enabled {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer sending paused",
            ));
        }
        state.sequence += 1;
        let sequence = state.sequence;
        let entry = state.entries.get_mut(owner).unwrap();
        entry.control = Some(Pending {
            body,
            sequence,
            attempt: 0,
            due: Instant::now(),
        });
        entry.control_blocked = true;
        self.inner.shared.wake.notify_all();
        Ok(())
    }
    /// Revoking a grant also revokes callbacks introduced under that grant.
    pub fn remove_owner(&self, owner_id: &str) -> io::Result<()> {
        self.retain_matching(|key| !owned_by(key, owner_id))
    }
    /// Reconcile callbacks with the authoritative grant list (including introduced peers).
    pub fn retain_owners(&self, owners: &[String]) -> io::Result<()> {
        self.retain_matching(|key| owners.iter().any(|owner| owned_by(key, owner)))
    }
    fn retain_matching(&self, keep: impl Fn(&str) -> bool) -> io::Result<()> {
        let mut state = self.inner.shared.state.lock().unwrap();
        let mut entries = state.entries.clone();
        entries.retain(|key, _| keep(key));
        if entries.len() == state.entries.len() && !state.persistence_dirty {
            return Ok(());
        }
        // Serialize disk and memory mutation with registration/publish. On save failure
        // fail closed in memory as well: revoked credentials must never keep retrying.
        let result = self.save(&entries);
        state.persistence_dirty = result.is_err();
        for (owner, socket) in state.sockets.iter().flatten() {
            if !entries.contains_key(owner) {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        state.entries = entries;
        self.inner.shared.wake.notify_all();
        result
    }
    pub fn back(&self, owner_id: &str) -> Option<Peer> {
        self.inner
            .shared
            .state
            .lock()
            .unwrap()
            .entries
            .get(owner_id)
            .map(|e| e.peer.clone())
    }
    pub fn failed(&self, owner_id: &str) -> bool {
        self.inner
            .shared
            .state
            .lock()
            .unwrap()
            .entries
            .get(owner_id)
            .is_some_and(|e| e.failed)
    }
    /// Pause outbound sync without retaining clipboard copies for a later resume.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.inner.shared.state.lock().unwrap();
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        if !enabled {
            state.sequence += 1;
            let generation = state.sequence;
            for entry in state.entries.values_mut() {
                entry.pending = None;
                entry.control = None;
                entry.busy = false;
                entry.generation = generation;
            }
            for (_, socket) in state.sockets.iter().flatten() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        self.inner.shared.wake.notify_all();
    }
    pub fn publish(&self, profile: Value) {
        self.publish_except(profile, None);
    }
    pub fn publish_except(&self, profile: Value, excluded_owner: Option<&str>) {
        let mut state = self.inner.shared.state.lock().unwrap();
        if !state.enabled {
            return;
        }
        let excluded_id = excluded_owner
            .and_then(|o| state.entries.get(o))
            .map(|e| e.peer.id.clone());
        state.sequence += 1;
        let sequence = state.sequence;
        for (owner, entry) in &mut state.entries {
            if excluded_owner == Some(owner.as_str())
                || excluded_id.as_ref() == Some(&entry.peer.id)
                || excluded_owner == Some(entry.peer.id.as_str())
            {
                continue;
            }
            entry.pending = Some(Pending {
                body: profile.clone(),
                sequence,
                attempt: 0,
                due: Instant::now(),
            });
        }
        self.inner.shared.wake.notify_all();
    }
    fn save(&self, entries: &BTreeMap<String, Entry>) -> io::Result<()> {
        let Some(path) = &self.inner.path else {
            return Ok(());
        };
        reject_symlinks(path)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        reject_symlinks(path)?;
        let values: BTreeMap<_, _> = entries.iter().map(|(k, v)| (k, &v.peer)).collect();
        let bytes = serde_json::to_vec(&values)?;
        // Unique adjacent temporary, mode set at creation: secrets never have a world-readable window.
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}
fn worker(shared: Arc<Shared>, slot: usize) {
    loop {
        let (owner, generation, peer, pending, control) = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.stop {
                    return;
                }
                if !state.enabled {
                    state = shared.wake.wait(state).unwrap();
                    continue;
                }
                let now = Instant::now();
                let next = |e: &Entry| {
                    e.control.as_ref().map(|p| (p.due, true)).or_else(|| {
                        if e.control_blocked {
                            None
                        } else {
                            e.pending.as_ref().map(|p| (p.due, false))
                        }
                    })
                };
                let ready = state.entries.iter().find_map(|(k, e)| {
                    if !e.busy {
                        next(e)
                            .filter(|(due, _)| *due <= now)
                            .map(|(_, control)| (k.clone(), control))
                    } else {
                        None
                    }
                });
                if let Some((owner, control)) = ready {
                    let e = state.entries.get_mut(&owner).unwrap();
                    e.busy = true;
                    let pending = if control {
                        e.control.take()
                    } else {
                        e.pending.take()
                    }
                    .unwrap();
                    break (owner, e.generation, e.peer.clone(), pending, control);
                }
                let delay = state
                    .entries
                    .values()
                    .filter(|e| !e.busy)
                    .filter_map(next)
                    .map(|(due, _)| due.saturating_duration_since(now))
                    .min();
                state = match delay {
                    Some(d) => shared.wake.wait_timeout(state, d).unwrap().0,
                    None => shared.wake.wait(state).unwrap(),
                };
            }
        };
        let result = request(
            &peer,
            if control { "POST" } else { "PUT" },
            if control {
                "/api/peer"
            } else {
                "/api/clipboard"
            },
            &pending.body,
            Some((&shared, slot, &owner, generation)),
        );
        let mut state = shared.state.lock().unwrap();
        state.sockets[slot] = None;
        if let Some(e) = state
            .entries
            .get_mut(&owner)
            .filter(|e| e.generation == generation)
        {
            e.busy = false;
            let success = result
                .as_ref()
                .is_ok_and(|(status, _)| (200..300).contains(status));
            e.failed = !success;
            let permanent = result
                .as_ref()
                .is_ok_and(|(s, _)| matches!(s, 400 | 401 | 403 | 413 | 415));
            let queue = if control {
                &mut e.control
            } else {
                &mut e.pending
            };
            if !success
                && !permanent
                && queue
                    .as_ref()
                    .is_none_or(|p| p.sequence <= pending.sequence)
            {
                let delays = [30, 120, 600];
                let mut retry = pending;
                retry.due = Instant::now() + Duration::from_secs(delays[retry.attempt.min(2)]);
                retry.attempt = (retry.attempt + 1).min(2);
                *queue = Some(retry);
            }
            if control && success && e.control.is_none() {
                e.control_blocked = false;
            }
            if control && permanent && e.control.is_none() {
                e.pending = None;
            }
        }
        shared.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload() -> Value {
        serde_json::json!({"v":2,"id":uuid::Uuid::new_v4().to_string(),"name":"test","origin":"https://127.0.0.1:443","port":443,"fingerprint":"ab".repeat(32),"token":URL_SAFE_NO_PAD.encode([7u8;32])})
    }
    fn server() -> (Arc<tiny_http::Server>, Peer) {
        let cert = rcgen::generate_simple_self_signed(vec!["not-the-ip.invalid".into()]).unwrap();
        let server = Arc::new(
            tiny_http::Server::https(
                "127.0.0.1:0",
                tiny_http::SslConfig {
                    certificate: cert.cert.pem().into_bytes(),
                    private_key: cert.key_pair.serialize_pem().into_bytes(),
                },
            )
            .unwrap(),
        );
        let mut value = payload();
        value["origin"] = format!("https://{}", server.server_addr().to_ip().unwrap()).into();
        value["fingerprint"] = hex::encode(Sha256::digest(cert.cert.der())).into();
        (server, Peer::introduction(&value).unwrap())
    }
    #[test]
    fn validation_and_source_binding() {
        let mut v = payload();
        for origin in [
            "http://192.168.1.1:443",
            "https://8.8.8.8:443",
            "https://example.org:443",
            "https://u@127.0.0.1:443",
            "https://127.0.0.1:443/",
            "https://[::ffff:8.8.8.8]:443",
            "https://127.0.0.1:0",
            "https://127.0.0.1:443?x",
            "https://[2001:4860:4860::8888]:443",
        ] {
            v["origin"] = origin.into();
            assert!(Peer::introduction(&v).is_err(), "{origin}");
        }
        let p = Peer::registration(&v, "192.168.4.5".parse().unwrap()).unwrap();
        assert_eq!(p.origin, "https://192.168.4.5:443");
        assert!(Peer::registration(&v, "8.8.8.8".parse().unwrap()).is_err());
        v["id"] = "not-a-uuid".into();
        assert!(Peer::registration(&v, "127.0.0.1".parse().unwrap()).is_err());
        assert!(!format!("{p:?}").contains(&p.token));
    }
    #[test]
    fn real_tls_pin_and_post() {
        let (server, peer) = server();
        let s = server.clone();
        let token = peer.token.clone();
        let handle =
            thread::spawn(move || {
                let mut req = s.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(req.method().as_str(), "POST");
                assert_eq!(req.url(), "/api/grant");
                assert!(req.headers().iter().any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == format!("Bearer {token}")));
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["v"], 2);
                req.respond(tiny_http::Response::from_string("{\"v\":2}"))
                    .unwrap();
            });
        let result = post_json(&peer, "/api/grant", &serde_json::json!({"v":2})).unwrap();
        assert_eq!(result, (200, Some(serde_json::json!({"v":2}))));
        handle.join().unwrap();
        let mut wrong = peer;
        wrong.fingerprint = "00".repeat(32);
        assert!(post_json(&wrong, "/api/grant", &serde_json::json!({})).is_err());
        server.unblock();
    }
    #[test]
    fn asynchronous_publish_retry_latest_and_shutdown() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        manager.register("owner", peer.clone()).unwrap();
        let start = Instant::now();
        manager.publish(serde_json::json!({"stamp":1}));
        assert!(start.elapsed() < Duration::from_millis(100));
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.method().as_str(), "PUT");
        request.respond(tiny_http::Response::empty(500)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !manager.failed("owner") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(manager.failed("owner"));
        manager.publish(serde_json::json!({"stamp":2}));
        let mut request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["stamp"], 2);
        // Drop while a request waits for its response, not merely during an idle retry.
        let start = Instant::now();
        drop(manager);
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(request);
        server.unblock();
    }
    #[test]
    fn permanent_failure_does_not_queue_retry() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        manager.register("owner", peer).unwrap();
        manager.publish(serde_json::json!({"stamp": 1}));
        let req = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        req.respond(tiny_http::Response::from_string("denied").with_status_code(401))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !manager.failed("owner") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(manager.failed("owner"));
        assert!(manager.inner.shared.state.lock().unwrap().entries["owner"]
            .pending
            .is_none());
        manager.remove_owner("owner").unwrap();
        assert!(!manager.failed("owner"));
        server.unblock();
    }
    #[test]
    fn trusted_peer_pin_cannot_be_replaced_by_same_or_different_owner() {
        let manager = PeerManager::open(None).unwrap();
        let peer = Peer::introduction(&payload()).unwrap();
        manager.register("owner", peer.clone()).unwrap();
        let mut impostor = peer.clone();
        impostor.fingerprint = "00".repeat(32);
        for owner in ["owner", "introduced:other:peer"] {
            assert_eq!(
                manager
                    .register(owner, impostor.clone())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        impostor.id = impostor.id.to_ascii_uppercase();
        assert!(manager.register("case-alias", impostor).is_err());
        assert_eq!(manager.back("owner").unwrap().fingerprint, peer.fingerprint);
        assert!(manager.back("introduced:other:peer").is_none());
        let mut updated = peer.clone();
        updated.origin = "https://192.168.1.8:18443".into();
        updated.token = URL_SAFE_NO_PAD.encode([19u8; 32]);
        manager.register("owner", updated.clone()).unwrap();
        let saved = manager.back("owner").unwrap();
        assert_eq!(saved.origin, updated.origin);
        assert_eq!(saved.token, updated.token);
        manager.register("introduced:other:peer", updated).unwrap();
        assert_eq!(
            manager.back("introduced:other:peer").unwrap().fingerprint,
            peer.fingerprint
        );
    }
    #[test]
    fn grant_reconciliation_cascades_and_persists() {
        let path = std::env::temp_dir().join(format!("peer-retain-{}", uuid::Uuid::new_v4()));
        let peer = Peer::introduction(&payload()).unwrap();
        let manager = PeerManager::open(Some(path.clone())).unwrap();
        for owner in [
            "a",
            "introduced:a:one",
            "introduced:ab:two",
            "ab",
            "introduced:b:one",
        ] {
            manager.register(owner, peer.clone()).unwrap();
        }
        // Queue retry work without any network dependency.
        {
            let mut state = manager.inner.shared.state.lock().unwrap();
            for entry in state.entries.values_mut() {
                entry.pending = Some(Pending {
                    body: serde_json::json!({}),
                    sequence: 1,
                    attempt: 1,
                    due: Instant::now() + Duration::from_secs(600),
                });
            }
        }
        manager.retain_owners(&["a".into(), "ab".into()]).unwrap();
        assert!(manager.back("introduced:b:one").is_none());
        manager.remove_owner("a").unwrap();
        assert!(manager.back("a").is_none());
        assert!(manager.back("introduced:a:one").is_none());
        assert!(manager.back("introduced:ab:two").is_some());
        drop(manager);
        let manager = PeerManager::open(Some(path.clone())).unwrap();
        assert!(manager.back("introduced:a:one").is_none());
        assert!(manager.back("introduced:ab:two").is_some());
        manager.retain_owners(&[]).unwrap();
        assert!(manager
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .entries
            .is_empty());
        drop(manager);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn owned_posts_use_cancellable_mesh_slot_and_snapshot_is_independent() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        assert_eq!(
            manager
                .post_owned("missing", "/api/grant", &payload())
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        for pause in [false, true] {
            manager.register("owner", peer.clone()).unwrap();
            let mut snapshot = manager.snapshot();
            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot[0].0, "owner");
            assert_eq!(snapshot[0].1.token, peer.token);
            snapshot[0].1.token.clear();
            assert_eq!(manager.back("owner").unwrap().token, peer.token);
            let cloned = manager.clone();
            let job = thread::spawn(move || {
                cloned.post_owned(
                    "owner",
                    "/api/grant",
                    &serde_json::json!({"client_id":"mesh"}),
                )
            });
            let mut req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(req.url(), "/api/grant");
            assert_eq!(req.method(), &tiny_http::Method::Post);
            let mut bytes = Vec::new();
            req.as_reader().read_to_end(&mut bytes).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap()["client_id"],
                "mesh"
            );
            assert!(manager.inner.shared.state.lock().unwrap().sockets[WORKERS].is_some());
            if pause {
                manager.set_enabled(false);
            } else {
                manager.remove_owner("owner").unwrap();
            }
            assert_eq!(
                job.join().unwrap().unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
            assert!(manager.inner.shared.state.lock().unwrap().sockets[WORKERS].is_none());
            drop(req);
            if pause {
                manager.set_enabled(true);
            }
        }
        assert!(server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none());
        server.unblock();
    }
    #[test]
    fn deadline_stream_expiry_and_trickling_reads_have_absolute_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut remote, _) = listener.accept().unwrap();
        let mut stream = DeadlineStream {
            socket,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert_eq!(
            stream.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            stream.write(b"x").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(stream.flush().unwrap_err().kind(), io::ErrorKind::TimedOut);
        let writer = thread::spawn(move || {
            for _ in 0..10 {
                let _ = remote.write_all(b"x");
                thread::sleep(Duration::from_millis(10));
            }
        });
        let started = Instant::now();
        stream.deadline = started + Duration::from_millis(35);
        let mut count = 0;
        while stream.read(&mut [0]).is_ok() {
            count += 1;
        }
        assert!(count > 0, "trickle did not deliver initial data");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(count < 10, "trickle extended the overall deadline");
        drop(stream);
        writer.join().unwrap();
    }
    #[test]
    fn reverse_registration_has_its_own_latest_slot_and_precedes_clipboard() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        manager.register("owner", peer.clone()).unwrap();
        let secret = URL_SAFE_NO_PAD.encode([23u8; 32]);
        let mut body = payload();
        body["reflected_token"] = secret.clone().into();
        manager.register_back("owner", body.clone()).unwrap();
        let mut first = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(first.method(), &tiny_http::Method::Post);
        assert_eq!(first.url(), "/api/peer");
        let mut bytes = Vec::new();
        first.as_reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
        // While control is in flight, neither clipboard nor a newer control
        // can overwrite the other's slot; the failed old control cannot return.
        manager.publish(serde_json::json!({"stamp": 1}));
        body["name"] = "new control".into();
        manager.register_back("owner", body.clone()).unwrap();
        manager.publish(serde_json::json!({"stamp": 2}));
        first.respond(tiny_http::Response::empty(500)).unwrap();
        let mut second = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(second.url(), "/api/peer");
        bytes.clear();
        second.as_reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
        assert!(!format!("{:?}", peer).contains(&peer.token));
        assert!(!format!("{:?}", peer).contains(&secret));
        manager.publish(serde_json::json!({"stamp": 3}));
        second.respond(tiny_http::Response::empty(200)).unwrap();
        let mut clipboard = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(clipboard.method(), &tiny_http::Method::Put);
        assert_eq!(clipboard.url(), "/api/clipboard");
        bytes.clear();
        clipboard.as_reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["stamp"], 3);
        clipboard.respond(tiny_http::Response::empty(200)).unwrap();
        assert!(server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        server.unblock();
    }
    #[test]
    fn revoke_and_pause_cancel_reverse_registration_and_do_not_resurrect_tokens() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        assert_eq!(
            manager
                .register_back("missing", payload())
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        for pause in [false, true] {
            manager.register("owner", peer.clone()).unwrap();
            manager.register_back("owner", payload()).unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            manager.publish(serde_json::json!({"stamp": 1}));
            if pause {
                manager.set_enabled(false);
            } else {
                manager.remove_owner("owner").unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while manager
                .inner
                .shared
                .state
                .lock()
                .unwrap()
                .sockets
                .iter()
                .any(Option::is_some)
            {
                assert!(Instant::now() < deadline, "control did not cancel");
                thread::sleep(Duration::from_millis(5));
            }
            drop(request);
            if pause {
                assert_eq!(
                    manager
                        .register_back("owner", payload())
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::PermissionDenied
                );
                let state = manager.inner.shared.state.lock().unwrap();
                assert!(state.entries["owner"].control.is_none());
                assert!(state.entries["owner"].pending.is_none());
                assert!(state.entries["owner"].control_blocked);
                drop(state);
                manager.set_enabled(true);
                manager.publish(serde_json::json!({"stamp": 2}));
            }
            assert!(server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none());
        }
        server.unblock();
    }
    #[test]
    fn pause_discards_retries_and_cancels_active_without_replaying_on_resume() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        manager.register("owner", peer).unwrap();
        manager.publish(serde_json::json!({"stamp": 1}));
        server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .respond(tiny_http::Response::empty(500))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !manager.failed("owner") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(manager.failed("owner"));
        manager.set_enabled(false);
        let generation = manager.inner.shared.state.lock().unwrap().sequence;
        manager.set_enabled(false);
        assert_eq!(
            manager.inner.shared.state.lock().unwrap().sequence,
            generation
        );
        manager.publish(serde_json::json!({"stamp": 2}));
        assert!(manager.inner.shared.state.lock().unwrap().entries["owner"]
            .pending
            .is_none());
        manager.set_enabled(true);
        assert!(server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        manager.publish(serde_json::json!({"stamp": 3}));
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        manager.set_enabled(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if manager
                .inner
                .shared
                .state
                .lock()
                .unwrap()
                .sockets
                .iter()
                .all(Option::is_none)
            {
                break;
            }
            assert!(Instant::now() < deadline, "paused callback did not cancel");
            thread::sleep(Duration::from_millis(5));
        }
        drop(request);
        manager.set_enabled(true);
        assert!(server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        assert!(manager.inner.shared.state.lock().unwrap().entries["owner"]
            .pending
            .is_none());
        manager.publish(serde_json::json!({"stamp": 4}));
        server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .respond(tiny_http::Response::empty(200))
            .unwrap();
        server.unblock();
    }
    #[test]
    fn retain_cancels_active_callback() {
        let (server, peer) = server();
        let manager = PeerManager::open(None).unwrap();
        manager.register("introduced:owner:peer", peer).unwrap();
        manager.publish(serde_json::json!({}));
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        manager.retain_owners(&[]).unwrap();
        assert!(manager.back("introduced:owner:peer").is_none());
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if manager
                .inner
                .shared
                .state
                .lock()
                .unwrap()
                .sockets
                .iter()
                .all(Option::is_none)
            {
                break;
            }
            assert!(Instant::now() < deadline, "revoked request did not cancel");
            thread::sleep(Duration::from_millis(5));
        }
        drop(request);
        server.unblock();
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_read_and_write() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("peer-links-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, &link).unwrap();
        assert!(PeerManager::open(Some(link.clone())).is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let manager = PeerManager::open(Some(root.join("future"))).unwrap();
        symlink(&target, root.join("future")).unwrap();
        assert!(manager
            .register("owner", Peer::introduction(&payload()).unwrap())
            .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        drop(manager);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn store_permissions_roundtrip_and_echo_exclusion() {
        let path = std::env::temp_dir().join(format!("peer-store-{}", uuid::Uuid::new_v4()));
        let peer = Peer::introduction(&payload()).unwrap();
        {
            let manager = PeerManager::open(Some(path.clone())).unwrap();
            manager.register("owner", peer.clone()).unwrap();
            manager.register("alias", peer.clone()).unwrap();
            manager.publish_except(serde_json::json!({"stamp":1}), Some("owner"));
            assert!(manager
                .inner
                .shared
                .state
                .lock()
                .unwrap()
                .entries
                .values()
                .all(|e| e.pending.is_none()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let manager = PeerManager::open(Some(path.clone())).unwrap();
        assert_eq!(manager.back("owner").unwrap().token, peer.token);
        manager.remove_owner("owner").unwrap();
        drop(manager);
        assert!(PeerManager::open(Some(path.clone()))
            .unwrap()
            .back("owner")
            .is_none());
        fs::remove_file(path).unwrap();
    }
}
