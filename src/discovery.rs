//! Nearby discovery is not authentication. Only a same-user CLI decision can approve a request.
//! Persist credentials BEFORE `complete`; a failed credential write can retry the approval.
//! After complete, the proof is a tombstone, never a second credential grant. HTTP callers
//! must serialize request/save/complete and must not expose `decide` as a network endpoint.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::IpAddr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const TTL: u64 = 120_000;
const SERVICE: &str = "_cyime-clip._tcp.local.";

pub struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}
impl Discovery {
    pub fn advertise(
        id: &str,
        name: &str,
        fingerprint: &str,
        port: u16,
        addresses: &[IpAddr],
    ) -> Result<Self, String> {
        let info = service_info(id, name, fingerprint, port, addresses)?;
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        if let Err(e) = daemon.register(info) {
            let _ = daemon.shutdown();
            return Err(e.to_string());
        }
        Ok(Self { daemon, fullname })
    }
}
impl Drop for Discovery {
    fn drop(&mut self) {
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(std::time::Duration::from_secs(1));
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}
fn service_info(
    id: &str,
    name: &str,
    fp: &str,
    port: u16,
    addresses: &[IpAddr],
) -> Result<ServiceInfo, String> {
    let id = Uuid::parse_str(id)
        .map_err(|_| "invalid server UUID")?
        .to_string();
    if !valid_fp(fp) || port == 0 || addresses.is_empty() || addresses.iter().any(|a| !local(*a)) {
        return Err("invalid discovery parameters".into());
    }
    let name = clean_name(name);
    // Android currently filters out kind=linux. This compatibility label is NOT a
    // claim of full Windows interoperability: only Android's inspected protocol is implemented.
    let props = [
        ("v", "2"),
        ("kind", "windows"),
        ("id", id.as_str()),
        ("name", name.as_str()),
        ("fp", fp),
    ];
    ServiceInfo::new(
        SERVICE,
        &format!("CyIME-{id}"),
        &format!("cyime-{}.local.", id.replace('-', "")),
        addresses,
        port,
        &props[..],
    )
    .map_err(|e| e.to_string())
}
fn valid_fp(fp: &str) -> bool {
    fp.len() == 64
        && fp
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn clean_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    if s.trim().is_empty() {
        "CyIME device".into()
    } else {
        s
    }
}
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private() || a.is_loopback() || a.is_link_local(),
        IpAddr::V6(a) => a
            .to_ipv4_mapped()
            .map(|a| local(a.into()))
            .unwrap_or_else(|| a.is_loopback() || a.is_unique_local() || a.is_unicast_link_local()),
    }
}
fn code(fp: &str, proof: &str) -> String {
    let hash = Sha256::digest(format!("cyime-nearby-v1\n{fp}\n{proof}"));
    format!(
        "{:08}",
        u32::from_be_bytes(hash[..4].try_into().unwrap()) % 100_000_000
    )
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub request_id: String,
    pub code: String,
    pub name: String,
    pub client_id: String,
    pub source: IpAddr,
    pub expires_at: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum NearbyResult {
    Pending {
        request_id: String,
        code: String,
        name: String,
    },
    Approved {
        request_id: String,
        client_id: String,
        name: String,
    },
}
struct Attempt {
    public: PendingRequest,
    proof: String,
}
pub struct Nearby {
    directory: PathBuf,
    fingerprint: String,
    attempts: Vec<Attempt>,
    dead: VecDeque<(String, String, u64)>,
    starts: VecDeque<(IpAddr, u64)>,
    last_now: u64,
}
impl Nearby {
    pub fn open(directory: Option<PathBuf>, fingerprint: String) -> io::Result<Self> {
        if !valid_fp(&fingerprint) {
            return Err(invalid("invalid certificate fingerprint"));
        }
        let directory = directory.unwrap_or_else(default_directory);
        secure_dir(&directory, true)?;
        Ok(Self {
            directory,
            fingerprint,
            attempts: Vec::new(),
            dead: VecDeque::new(),
            starts: VecDeque::new(),
            last_now: 0,
        })
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn request(
        &mut self,
        body: &Value,
        source: IpAddr,
        now_ms: u64,
    ) -> Result<NearbyResult, String> {
        if now_ms < self.last_now {
            return Err("clock moved backwards".into());
        }
        self.last_now = now_ms;
        self.expire(now_ms);
        if !local(source) {
            return Err("nearby pairing requires a local source address".into());
        }
        if body.get("v").and_then(Value::as_u64) != Some(2) {
            return Err("expected v=2".into());
        }
        let client_id = body
            .get("client_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or("invalid client UUID")?
            .to_string();
        let proof = body
            .get("proof")
            .or_else(|| body.get("secret"))
            .and_then(Value::as_str)
            .ok_or("missing proof")?;
        if body.get("proof").is_some()
            && body.get("secret").is_some()
            && body["proof"] != body["secret"]
        {
            return Err("conflicting proof fields".into());
        }
        if proof.len() != 43
            || URL_SAFE_NO_PAD
                .decode(proof)
                .map(|b| b.len() != 32 || URL_SAFE_NO_PAD.encode(b) != proof)
                .unwrap_or(true)
        {
            return Err("invalid proof".into());
        }
        let raw_name = body
            .get("client_name")
            .and_then(Value::as_str)
            .ok_or("missing client name")?;
        if raw_name.len() > 512 {
            return Err("client name too long".into());
        }
        if self
            .dead
            .iter()
            .any(|(id, p, _)| id == &client_id && p == proof)
        {
            return Err("pairing request expired, rejected or completed".into());
        }
        if let Some(a) = self
            .attempts
            .iter()
            .find(|a| a.public.client_id == client_id && a.proof == proof)
        {
            if a.public.source != source {
                return Err("pairing source changed".into());
            }
            let p = &a.public;
            let decision =
                read_decision(&self.directory, &p.request_id).map_err(|e| e.to_string())?;
            if decision == Some(false) {
                let id = p.request_id.clone();
                self.retire(&id, now_ms);
                return Err("pairing rejected locally".into());
            }
            return Ok(if decision == Some(true) {
                NearbyResult::Approved {
                    request_id: p.request_id.clone(),
                    client_id,
                    name: p.name.clone(),
                }
            } else {
                NearbyResult::Pending {
                    request_id: p.request_id.clone(),
                    code: p.code.clone(),
                    name: p.name.clone(),
                }
            });
        }
        while self
            .starts
            .front()
            .is_some_and(|(_, t)| now_ms.saturating_sub(*t) >= 60_000)
        {
            self.starts.pop_front();
        }
        if self.attempts.len() >= 8 {
            return Err("too many pending requests".into());
        }
        if self.starts.len() >= 16
            || self.starts.iter().filter(|(ip, _)| *ip == source).count() >= 4
        {
            return Err("nearby pairing rate limited".into());
        }
        let p = PendingRequest {
            request_id: Uuid::new_v4().to_string(),
            code: code(&self.fingerprint, proof),
            name: clean_name(raw_name),
            client_id,
            source,
            expires_at: now_ms.saturating_add(TTL),
        };
        atomic_json(&self.directory, &format!("{}.pending", p.request_id), &p)
            .map_err(|e| e.to_string())?;
        self.starts.push_back((source, now_ms));
        let result = NearbyResult::Pending {
            request_id: p.request_id.clone(),
            code: p.code.clone(),
            name: p.name.clone(),
        };
        self.attempts.push(Attempt {
            public: p,
            proof: proof.into(),
        });
        Ok(result)
    }
    /// Call only after durable credential storage succeeds. No network retry may mint a second token.
    pub fn complete(&mut self, request_id: &str) -> io::Result<()> {
        valid_id(request_id)?;
        if !self
            .attempts
            .iter()
            .any(|a| a.public.request_id == request_id)
        {
            return Err(invalid("unknown pairing request"));
        }
        if read_decision(&self.directory, request_id)? != Some(true) {
            return Err(invalid("request is not locally approved"));
        }
        self.retire(request_id, self.last_now);
        Ok(())
    }
    fn retire(&mut self, id: &str, now: u64) {
        if let Some(i) = self.attempts.iter().position(|a| a.public.request_id == id) {
            let a = self.attempts.remove(i);
            self.dead
                .push_back((a.public.client_id, a.proof, now.saturating_add(TTL)));
            let _ = fs::remove_file(self.directory.join(format!("{id}.pending")));
            let _ = fs::remove_file(self.directory.join(format!("{id}.decision")));
        }
    }
    fn expire(&mut self, now: u64) {
        let ids: Vec<_> = self
            .attempts
            .iter()
            .filter(|a| now >= a.public.expires_at)
            .map(|a| a.public.request_id.clone())
            .collect();
        for id in ids {
            self.retire(&id, now);
        }
        while self.dead.front().is_some_and(|(_, _, t)| now >= *t) {
            self.dead.pop_front();
        }
    }
}

pub fn default_directory() -> PathBuf {
    // The CLI and HTTPS listener must inspect the same private approval files.
    crate::config::state_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("cyime-wayclip-{}", uid())))
        .join("nearby")
}

fn uid() -> u32 {
    fs::metadata("/proc/self")
        .expect("Linux procfs required")
        .uid()
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn valid_id(id: &str) -> io::Result<()> {
    if Uuid::parse_str(id)
        .map(|u| u.to_string() == id)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(invalid("invalid request_id"))
    }
}
fn secure_dir(dir: &Path, create: bool) -> io::Result<()> {
    // Reject symlink components, including a symlink supplied as the runtime directory.
    // An attacker cannot swap a child of the final 0700, same-uid directory.
    let absolute = if dir.is_absolute() {
        dir.to_owned()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let mut current = PathBuf::new();
    for part in absolute.components() {
        if matches!(part, std::path::Component::ParentDir) {
            return Err(invalid("parent directory components are not allowed"));
        }
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(invalid("runtime path must contain only real directories"))
            }
            Ok(m) => {
                // Every ancestor must resist renames by another uid. Root-owned
                // sticky /tmp is safe; arbitrary shared writable directories are not.
                if (current != Path::new("/") && m.uid() != uid() && m.uid() != 0)
                    || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
                {
                    return Err(invalid("unsafe writable runtime ancestor"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != uid() || m.permissions().mode() & 0o077 != 0 {
        return Err(invalid(
            "runtime directory must be owned by this user and mode 0700",
        ));
    }
    Ok(())
}
fn private_read(path: &Path) -> io::Result<Vec<u8>> {
    // Linux O_NOFOLLOW: never follow a malicious approval/pending symlink.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0 || m.nlink() != 1 || m.len() > 8192
    {
        return Err(invalid("unsafe local pairing file"));
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(invalid("local pairing file too large"));
    }
    Ok(bytes)
}
fn atomic_json(dir: &Path, name: &str, value: &impl Serialize) -> io::Result<()> {
    secure_dir(dir, false)?;
    let temporary = dir.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        // Publish without replacement: concurrent approve/reject cannot override
        // an earlier decision. A crash before unlink leaves an unreadable (nlink=2)
        // record, failing closed rather than silently granting consent.
        fs::hard_link(&temporary, dir.join(name))?;
        fs::remove_file(&temporary)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
#[derive(Serialize, Deserialize)]
struct Decision {
    request_id: String,
    approve: bool,
}
fn read_decision(dir: &Path, id: &str) -> io::Result<Option<bool>> {
    secure_dir(dir, false)?;
    match private_read(&dir.join(format!("{id}.decision"))) {
        Ok(bytes) => {
            let d: Decision = serde_json::from_slice(&bytes)?;
            if d.request_id != id {
                return Err(invalid("decision request_id mismatch"));
            }
            Ok(Some(d.approve))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn wall_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("clock before epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| invalid("clock overflow"))
}
/// Pending metadata contains no proof or credential. Expired and already decided entries are hidden.
pub fn pending(dir: &Path) -> io::Result<Vec<PendingRequest>> {
    secure_dir(dir, false)?;
    let now = wall_ms()?;
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".pending")) else {
            continue;
        };
        valid_id(id)?;
        let p: PendingRequest = match private_read(&entry.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if p.request_id != id {
            return Err(invalid("pending request_id mismatch"));
        }
        if now < p.expires_at && read_decision(dir, id)?.is_none() {
            result.push(p);
        }
    }
    result.sort_by(|a, b| a.request_id.cmp(&b.request_id));
    Ok(result)
}
/// Trusted local CLI only; approving a specific id asserts that the user compared its code.
pub fn decide(dir: &Path, id: &str, approve: bool) -> io::Result<()> {
    valid_id(id)?;
    secure_dir(dir, false)?;
    let p: PendingRequest =
        serde_json::from_slice(&private_read(&dir.join(format!("{id}.pending")))?)?;
    if p.request_id != id || wall_ms()? >= p.expires_at {
        return Err(invalid("pairing request expired or mismatched"));
    }
    if read_decision(dir, id)?.is_some() {
        return Err(invalid("pairing request already decided"));
    }
    atomic_json(
        dir,
        &format!("{id}.decision"),
        &Decision {
            request_id: id.into(),
            approve,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn setup() -> (Nearby, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cyime-test-{}", Uuid::new_v4()));
        (
            Nearby::open(Some(dir.clone()), "ab".repeat(32)).unwrap(),
            dir,
        )
    }
    fn body(n: u8) -> Value {
        json!({"v":2,"client_id":"00000000-0000-4000-8000-000000000001", "client_name":"phone\n", "secret":URL_SAFE_NO_PAD.encode([n;32])})
    }
    fn ip(n: u8) -> IpAddr {
        format!("192.168.1.{n}").parse().unwrap()
    }
    fn id(r: NearbyResult) -> String {
        match r {
            NearbyResult::Pending { request_id, .. } => request_id,
            _ => panic!(),
        }
    }
    #[test]
    fn android_hash_vector() {
        // SHA-256 input is UTF-8, no trailing newline; Java ByteBuffer.int is unsigned here.
        assert_eq!(
            code(&"ab".repeat(32), &URL_SAFE_NO_PAD.encode([0u8; 32])),
            "41510565"
        );
        // High bit set: Java must mask its signed int to an unsigned long.
        assert_eq!(
            code(&"ab".repeat(32), &URL_SAFE_NO_PAD.encode([1u8; 32])),
            "88845578"
        );
    }
    #[test]
    fn consent_and_completion() {
        let (mut n, d) = setup();
        let now = wall_ms().unwrap();
        let b = body(0);
        let first = n.request(&b, ip(1), now).unwrap();
        assert_eq!(n.request(&b, ip(1), now + 1).unwrap(), first);
        assert!(n.request(&b, ip(2), now + 1).is_err());
        let id = id(first);
        assert!(n.complete(&id).is_err());
        assert_eq!(pending(&d).unwrap().len(), 1);
        assert!(
            !String::from_utf8(private_read(&d.join(format!("{id}.pending"))).unwrap())
                .unwrap()
                .contains("secret")
        );
        decide(&d, &id, true).unwrap();
        assert!(decide(&d, &id, false).is_err());
        assert!(matches!(
            n.request(&b, ip(1), now + 2).unwrap(),
            NearbyResult::Approved { .. }
        ));
        // Retrying a failed durable write retains consent.
        assert!(matches!(
            n.request(&b, ip(1), now + 3).unwrap(),
            NearbyResult::Approved { .. }
        ));
        n.complete(&id).unwrap();
        assert!(n.request(&b, ip(1), now + 4).is_err());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn expiry_reject_limits_and_validation() {
        let (mut n, d) = setup();
        let now = wall_ms().unwrap();
        let first = id(n.request(&body(0), ip(1), now).unwrap());
        decide(&d, &first, false).unwrap();
        assert!(n.request(&body(0), ip(1), now + 1).is_err());
        for i in 1..4 {
            n.request(&body(i), ip(1), now + 1).unwrap();
        }
        assert!(n
            .request(&body(4), ip(1), now + 1)
            .unwrap_err()
            .contains("rate limited"));
        for i in 4..9 {
            n.request(&body(i), ip(i), now + 1).unwrap();
        }
        assert!(n
            .request(&body(10), ip(10), now + 1)
            .unwrap_err()
            .contains("too many"));
        assert!(n.request(&body(1), ip(1), now + TTL + 1).is_err());
        assert!(n
            .request(&body(20), "8.8.8.8".parse().unwrap(), now + TTL + 1)
            .is_err());
        assert!(decide(&d, "../../evil", true).is_err());
        let mut bad = body(20);
        bad["secret"] = json!("a".repeat(43)); // noncanonical final base64 bits
        assert!(n.request(&bad, ip(1), now + TTL + 1).is_err());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn global_new_attempt_rate_limit() {
        let (mut n, d) = setup();
        let now = wall_ms().unwrap();
        for i in 0..16 {
            let b = body(i);
            let request_id = id(n.request(&b, ip(i + 1), now).unwrap());
            decide(&d, &request_id, false).unwrap();
            assert!(n.request(&b, ip(i + 1), now).is_err());
        }
        assert!(n
            .request(&body(17), ip(18), now)
            .unwrap_err()
            .contains("rate limited"));
        assert!(matches!(
            n.request(&body(17), ip(18), now + 60_000),
            Ok(NearbyResult::Pending { .. })
        ));
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn files_fail_closed() {
        use std::os::unix::fs::symlink;
        let (mut n, d) = setup();
        let now = wall_ms().unwrap();
        let id = id(n.request(&body(0), ip(1), now).unwrap());
        symlink("/etc/passwd", d.join(format!("{id}.decision"))).unwrap();
        assert!(n.request(&body(0), ip(1), now).is_err());
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(pending(&d).is_err());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn advertisement_fields() {
        let info = service_info(
            "00000000-0000-4000-8000-000000000001",
            "Linux",
            &"ab".repeat(32),
            8443,
            &[ip(1)],
        )
        .unwrap();
        assert_eq!(info.get_property_val_str("v"), Some("2"));
        assert_eq!(info.get_property_val_str("kind"), Some("windows"));
        assert!(info.get_property_val_str("token").is_none());
        assert!(info.get_property_val_str("secret").is_none());
    }
}
