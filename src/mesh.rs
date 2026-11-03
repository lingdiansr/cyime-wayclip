//! Explicit trust expansion between devices already paired with this host.
//! One bounded worker retains grant checkpoints in memory only; no secrets are logged.
use crate::peers::{Peer, PeerManager};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Control {
    enabled: bool,
    dirty: bool,
    stop: bool,
    epoch: u64,
}
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
}
pub struct MeshCoordinator {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    finished: Mutex<mpsc::Receiver<()>>,
}
impl MeshCoordinator {
    pub fn new(peers: PeerManager) -> Self {
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                enabled: false,
                dirty: false,
                stop: false,
                epoch: 0,
            }),
            wake: Condvar::new(),
        });
        let (tx, finished) = mpsc::channel();
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("cyime-mesh".into())
            .spawn(move || {
                coordinate(peers, state);
                let _ = tx.send(());
            })
            .expect("cannot start mesh coordinator");
        Self {
            shared,
            worker: Some(worker),
            finished: Mutex::new(finished),
        }
    }
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.shared.control.lock().unwrap();
        if state.enabled != enabled {
            state.enabled = enabled;
            state.epoch = state.epoch.wrapping_add(1);
            state.dirty = enabled;
            self.shared.wake.notify_all();
        }
    }
    /// Coalesced notification; never performs network I/O on the caller thread.
    pub fn trigger(&self) {
        let mut state = self.shared.control.lock().unwrap();
        if state.enabled && !state.stop {
            state.dirty = true;
            self.shared.wake.notify_one();
        }
    }
}
impl Drop for MeshCoordinator {
    fn drop(&mut self) {
        {
            let mut state = self.shared.control.lock().unwrap();
            state.stop = true;
            state.enabled = false;
            self.shared.wake.notify_all();
        }
        // A control RPC has an absolute 12-second deadline. Never let shutdown
        // block indefinitely even if a transport violates that contract.
        if self
            .finished
            .get_mut()
            .unwrap()
            .recv_timeout(Duration::from_secs(12))
            .is_ok()
        {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
#[derive(Clone)]
struct Member {
    owner: String,
    peer: Peer,
}
fn same(a: &Member, b: &Member) -> bool {
    a.owner == b.owner
        && a.peer.id == b.peer.id
        && a.peer.name == b.peer.name
        && a.peer.origin == b.peer.origin
        && a.peer.fingerprint == b.peer.fingerprint
        && a.peer.token == b.peer.token
}
fn members(peers: &PeerManager) -> Vec<Member> {
    let mut entries: Vec<_> = peers
        .snapshot()
        .into_iter()
        .take(32)
        .map(|(owner, peer)| Member { owner, peer })
        .collect();
    entries.sort_by(|a, b| (&a.peer.id, &a.owner).cmp(&(&b.peer.id, &b.owner)));
    entries.dedup_by(|a, b| a.peer.id == b.peer.id);
    entries
}
struct Job {
    a: Member,
    b: Member,
    // A's token for B, then B's token for A. They must cross in introductions.
    grant_a: Option<String>,
    grant_b: Option<String>,
    stage: u8,
    failures: usize,
    due: Instant,
    permanent: bool,
}
impl Job {
    fn new(a: Member, b: Member) -> Self {
        Self {
            a,
            b,
            grant_a: None,
            grant_b: None,
            stage: 0,
            failures: 0,
            due: Instant::now(),
            permanent: false,
        }
    }
}
fn valid(shared: &Shared, epoch: u64, peers: &PeerManager, job: &Job) -> bool {
    let state = shared.control.lock().unwrap();
    if state.stop || !state.enabled || state.epoch != epoch {
        return false;
    }
    drop(state);
    let entries = peers.snapshot();
    [&job.a, &job.b].iter().all(|expected| {
        entries.iter().any(|(owner, peer)| {
            same(
                expected,
                &Member {
                    owner: owner.clone(),
                    peer: peer.clone(),
                },
            )
        })
    })
}
enum Step {
    Success,
    Retry,
    Permanent,
    Invalid,
}
fn grant(value: Option<Value>) -> Option<String> {
    let value = value?;
    if value.get("v")?.as_u64()? != 2 {
        return None;
    }
    let token = value.get("token")?.as_str()?;
    if token.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != token {
        return None;
    }
    Some(token.to_owned())
}
fn introduce(member: &Member, token: &str) -> Value {
    json!({"v":2,"id":member.peer.id,"name":member.peer.name,"origin":member.peer.origin,"fingerprint":member.peer.fingerprint,"token":token})
}
fn step(peers: &PeerManager, shared: &Shared, epoch: u64, job: &mut Job) -> Step {
    if !valid(shared, epoch, peers, job) {
        return Step::Invalid;
    }
    let (owner, path, body) = match job.stage {
        0 => (
            &job.a.owner,
            "/api/grant",
            json!({"v":2,"client_id":job.b.peer.id,"name":job.b.peer.name}),
        ),
        1 => (
            &job.b.owner,
            "/api/grant",
            json!({"v":2,"client_id":job.a.peer.id,"name":job.a.peer.name}),
        ),
        2 => (
            &job.a.owner,
            "/api/introduce",
            introduce(&job.b, job.grant_b.as_deref().unwrap()),
        ),
        3 => (
            &job.b.owner,
            "/api/introduce",
            introduce(&job.a, job.grant_a.as_deref().unwrap()),
        ),
        _ => return Step::Success,
    };
    let response = peers.post_owned(owner, path, &body);
    if !valid(shared, epoch, peers, job) {
        return Step::Invalid;
    }
    let (status, value) = match response {
        Ok(r) => r,
        Err(_) => return Step::Retry,
    };
    if matches!(status, 400 | 401 | 403 | 409 | 413 | 415) {
        return Step::Permanent;
    }
    if !(200..300).contains(&status) {
        return Step::Retry;
    }
    if job.stage < 2 {
        let token = match grant(value) {
            Some(t) => t,
            None => return Step::Permanent,
        };
        if job.stage == 0 {
            job.grant_a = Some(token);
        } else {
            job.grant_b = Some(token);
        }
    }
    job.stage += 1;
    if job.stage == 4 {
        job.grant_a = None;
        job.grant_b = None;
    }
    Step::Success
}
fn coordinate(peers: PeerManager, shared: Arc<Shared>) {
    let mut jobs: BTreeMap<(String, String), Job> = BTreeMap::new();
    let mut epoch = 0;
    loop {
        let explicit = {
            let mut state = shared.control.lock().unwrap();
            if state.stop {
                return;
            }
            if state.epoch != epoch {
                jobs.retain(|_, job| job.stage == 4);
                epoch = state.epoch;
            }
            if !state.enabled {
                jobs.retain(|_, job| job.stage == 4);
                drop(shared.wake.wait(state).unwrap());
                continue;
            }
            let dirty = state.dirty;
            state.dirty = false;
            dirty
        };
        let current = members(&peers);
        jobs.retain(|_, j| {
            current.iter().any(|p| same(p, &j.a)) && current.iter().any(|p| same(p, &j.b))
        });
        for i in 0..current.len() {
            for j in i + 1..current.len() {
                let a = &current[i];
                let b = &current[j];
                jobs.entry((a.peer.id.clone(), b.peer.id.clone()))
                    .or_insert_with(|| Job::new(a.clone(), b.clone()));
            }
        }
        if explicit {
            for job in jobs.values_mut().filter(|j| j.permanent) {
                job.permanent = false;
                job.due = Instant::now();
            }
        }
        let now = Instant::now();
        let ready = jobs
            .iter()
            .filter(|(_, j)| j.stage < 4 && !j.permanent && j.due <= now)
            .min_by_key(|(_, j)| j.due)
            .map(|(key, _)| key.clone());
        if let Some(key) = ready {
            let job = jobs.get_mut(&key).unwrap();
            match step(&peers, &shared, epoch, job) {
                Step::Invalid => {
                    jobs.remove(&key);
                }
                Step::Success => {
                    job.failures = 0;
                    job.due = Instant::now();
                }
                Step::Permanent => {
                    job.permanent = true;
                }
                Step::Retry => {
                    let delays = [30, 120, 600];
                    job.due = Instant::now() + Duration::from_secs(delays[job.failures.min(2)]);
                    job.failures = job.failures.saturating_add(1);
                }
            }
            continue;
        }
        let state = shared.control.lock().unwrap();
        if state.stop {
            return;
        }
        if state.dirty || !state.enabled || state.epoch != epoch {
            continue;
        }
        let next = jobs
            .values()
            .filter(|j| j.stage < 4 && !j.permanent)
            .map(|j| j.due)
            .min();
        if let Some(next) = next {
            drop(
                shared
                    .wake
                    .wait_timeout(state, next.saturating_duration_since(Instant::now()))
                    .unwrap(),
            );
        } else {
            drop(shared.wake.wait(state).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicBool, Ordering};
    type Events = Arc<Mutex<Vec<(String, Value)>>>;
    type Hook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
    struct Mock {
        peer: Peer,
        events: Events,
        fail_intro: Arc<AtomicBool>,
        grant_hook: Hook,
        certificate: (Vec<u8>, Vec<u8>, String),
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }
    impl Mock {
        fn new(id: u8, token: u8, identity: Option<(Vec<u8>, Vec<u8>, String)>) -> Self {
            let certificate = identity.unwrap_or_else(|| {
                let cert = rcgen::generate_simple_self_signed(vec!["mesh.invalid".into()]).unwrap();
                (
                    cert.cert.pem().into_bytes(),
                    cert.key_pair.serialize_pem().into_bytes(),
                    hex::encode(Sha256::digest(cert.cert.der())),
                )
            });
            let server = Arc::new(
                tiny_http::Server::https(
                    "127.0.0.1:0",
                    tiny_http::SslConfig {
                        certificate: certificate.0.clone(),
                        private_key: certificate.1.clone(),
                    },
                )
                .unwrap(),
            );
            let peer = Peer {
                id: format!("00000000-0000-4000-8000-{id:012}"),
                name: format!("device {id}"),
                origin: format!("https://{}", server.server_addr().to_ip().unwrap()),
                fingerprint: certificate.2.clone(),
                token: URL_SAFE_NO_PAD.encode([token; 32]),
            };
            let events = Arc::new(Mutex::new(Vec::new()));
            let fail_intro = Arc::new(AtomicBool::new(false));
            let grant_hook: Hook = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));
            let (record, fail, hook, done) = (
                events.clone(),
                fail_intro.clone(),
                grant_hook.clone(),
                stop.clone(),
            );
            let worker = thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                    else {
                        continue;
                    };
                    let mut bytes = Vec::new();
                    request.as_reader().read_to_end(&mut bytes).unwrap();
                    let body: Value = serde_json::from_slice(&bytes).unwrap();
                    let path = request.url().to_owned();
                    record.lock().unwrap().push((path.clone(), body));
                    if path == "/api/grant" {
                        if let Some(hook) = hook.lock().unwrap().take() {
                            hook();
                        }
                    }
                    let status = if path == "/api/introduce" && fail.swap(false, Ordering::Relaxed)
                    {
                        409
                    } else {
                        200
                    };
                    let response = if path == "/api/grant" {
                        json!({"v":2,"token":URL_SAFE_NO_PAD.encode([token;32])})
                    } else {
                        json!({"v":2})
                    };
                    let _ = request.respond(
                        tiny_http::Response::from_string(response.to_string())
                            .with_status_code(status),
                    );
                }
            });
            Self {
                peer,
                events,
                fail_intro,
                grant_hook,
                certificate,
                stop,
                worker: Some(worker),
            }
        }
        fn count(&self) -> usize {
            self.events.lock().unwrap().len()
        }
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }
    fn wait(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "mesh coordinator did not progress"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn setup(a: &Mock, b: &Mock) -> PeerManager {
        let peers = PeerManager::open(None).unwrap();
        peers.register("a", a.peer.clone()).unwrap();
        peers.register("b", b.peer.clone()).unwrap();
        peers
    }
    #[test]
    fn tls_four_steps_cross_tokens_and_completed_signature_deduplicates() {
        let a = Mock::new(1, 7, None);
        let b = Mock::new(2, 8, None);
        let peers = setup(&a, &b);
        let mesh = MeshCoordinator::new(peers.clone());
        mesh.trigger();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(a.count() + b.count(), 0);
        mesh.set_enabled(true);
        mesh.trigger();
        wait(|| a.count() == 2 && b.count() == 2);
        for (target, other, token) in [(&a, &b, 8), (&b, &a, 7)] {
            let events = target.events.lock().unwrap();
            assert_eq!(events[0].0, "/api/grant");
            assert_eq!(
                events[0].1,
                json!({"v":2,"client_id":other.peer.id,"name":other.peer.name})
            );
            assert_eq!(events[1].0, "/api/introduce");
            assert_eq!(
                events[1].1,
                introduce(
                    &Member {
                        owner: String::new(),
                        peer: other.peer.clone()
                    },
                    &URL_SAFE_NO_PAD.encode([token; 32])
                )
            );
        }
        for _ in 0..20 {
            mesh.trigger();
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(a.count() + b.count(), 4);
        mesh.set_enabled(false);
        mesh.set_enabled(true);
        mesh.trigger();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(a.count() + b.count(), 4);
        let moved = Mock::new(1, 7, Some(a.certificate.clone()));
        peers.register("a", moved.peer.clone()).unwrap();
        mesh.trigger();
        wait(|| moved.count() == 2 && b.count() == 4);
        let events = b.events.lock().unwrap();
        assert_eq!(events[3].1["origin"], moved.peer.origin);
    }
    #[test]
    fn failed_introduction_keeps_successful_grant_checkpoints() {
        let a = Mock::new(1, 7, None);
        let b = Mock::new(2, 8, None);
        a.fail_intro.store(true, Ordering::Relaxed);
        let mesh = MeshCoordinator::new(setup(&a, &b));
        mesh.set_enabled(true);
        wait(|| a.count() == 2 && b.count() == 1);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(a.count() + b.count(), 3);
        mesh.trigger();
        wait(|| a.count() == 3 && b.count() == 2);
        for target in [&a, &b] {
            assert_eq!(
                target
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(path, _)| path == "/api/grant")
                    .count(),
                1
            );
        }
    }
    #[test]
    fn revocation_during_first_grant_prevents_all_subsequent_steps() {
        let a = Mock::new(1, 7, None);
        let b = Mock::new(2, 8, None);
        let peers = setup(&a, &b);
        let revoke = peers.clone();
        *a.grant_hook.lock().unwrap() = Some(Box::new(move || revoke.remove_owner("b").unwrap()));
        let mesh = MeshCoordinator::new(peers);
        mesh.set_enabled(true);
        wait(|| a.count() == 1);
        thread::sleep(Duration::from_millis(150));
        mesh.trigger();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 0);
    }
    #[test]
    fn malformed_grant_tokens_are_not_introduced() {
        for token in ["a".repeat(43), "!".repeat(43), "A".repeat(42)] {
            assert!(grant(Some(json!({"v":2,"token":token}))).is_none());
        }
        assert!(grant(Some(json!({"v":1,"token":URL_SAFE_NO_PAD.encode([8;32])}))).is_none());
    }
}
