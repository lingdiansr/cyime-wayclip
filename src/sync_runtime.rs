//! Serialized desktop state shared by polling HTTP clients and the push watcher.
use crate::{
    clipboard::ClipboardSnapshot,
    config::Config,
    peers::PeerManager,
    protocol::clipboard_wire::{parse_clipboard_body, ClipboardRequestError},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub type ClipboardRead = Box<dyn FnMut() -> Result<ClipboardSnapshot, String> + Send>;
pub type ClipboardWrite = Box<dyn FnMut(&str, u64) -> Result<(), String> + Send>;
pub type ClipboardEventWait = Box<dyn FnMut(Duration) -> bool + Send>;
struct Desktop {
    read: ClipboardRead,
    write: ClipboardWrite,
    value: Option<Value>,
    stamp: i64,
    legacy: HashMap<String, u64>,
    initialized: bool,
    last_hash: Option<String>,
    generation: u64,
    session: u64,
    clock: crate::clock_store::ClockStore,
}
impl Desktop {
    fn refresh(&mut self, limit: usize) -> Result<bool, String> {
        let snapshot = (self.read)()?;
        let generation = snapshot.generation;
        let session = snapshot.session;
        let text = snapshot
            .text
            .filter(|t| !t.trim().is_empty() && !t.contains('\0') && t.len() <= limit);
        // Generation advances on every selection event, even when the text is
        // unchanged. A new compositor session is not a new copy: re-baseline so a
        // pre-existing selection is never fanned out.
        self.generation = generation;
        if session != self.session {
            self.session = session;
            self.initialized = false;
        }
        let hash = text
            .as_deref()
            .map(|t| hex::encode(Sha256::digest(t.as_bytes())));
        if !self.initialized {
            // There is no Wayland copy timestamp for a pre-existing selection.
            // Restore a known hash's clock; otherwise keep an unknown baseline at 0.
            let stamp = if hash == self.last_hash {
                self.stamp
            } else {
                0
            };
            self.clock
                .save(crate::clock_store::ClockSnapshot {
                    hash: hash.clone(),
                    stamp,
                    legacy: self.legacy.clone(),
                })
                .map_err(|e| e.to_string())?;
            self.stamp = stamp;
            self.last_hash = hash;
            self.initialized = true;
            self.value = text.map(|t| profile(t.as_ref(), stamp));
            return Ok(false);
        }
        if text.as_deref() == self.value.as_ref().and_then(|v| v["text"].as_str()) {
            return Ok(false);
        }
        let stamp = now_ms().max(self.stamp.saturating_add(1));
        self.clock
            .save(crate::clock_store::ClockSnapshot {
                hash: hash.clone(),
                stamp,
                legacy: self.legacy.clone(),
            })
            .map_err(|e| e.to_string())?;
        self.stamp = stamp;
        self.last_hash = hash;
        self.value = text.map(|t| profile(t.as_ref(), self.stamp));
        Ok(true)
    }
}

pub struct SyncRuntime {
    desktop: Mutex<Desktop>,
    config: Mutex<Config>,
    config_path: Option<PathBuf>,
    pub peers: PeerManager,
    pub mesh: crate::mesh::MeshCoordinator,
    stop: Arc<AtomicBool>,
    host: Mutex<Option<(u16, String)>>,
}
impl SyncRuntime {
    #[cfg(test)]
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        peers: PeerManager,
        read: ClipboardRead,
        write: ClipboardWrite,
    ) -> Arc<Self> {
        Self::build(
            config,
            config_path,
            peers,
            read,
            write,
            crate::clock_store::ClockStore::open(None).unwrap(),
        )
    }
    pub fn persistent(
        config: Config,
        config_path: Option<PathBuf>,
        peers: PeerManager,
        read: ClipboardRead,
        write: ClipboardWrite,
        path: PathBuf,
    ) -> std::io::Result<Arc<Self>> {
        Ok(Self::build(
            config,
            config_path,
            peers,
            read,
            write,
            crate::clock_store::ClockStore::open(Some(path))?,
        ))
    }
    fn build(
        config: Config,
        config_path: Option<PathBuf>,
        peers: PeerManager,
        read: ClipboardRead,
        write: ClipboardWrite,
        clock: crate::clock_store::ClockStore,
    ) -> Arc<Self> {
        let snapshot = clock.snapshot();
        let mut desktop = Desktop {
            read,
            write,
            value: None,
            stamp: snapshot.stamp,
            legacy: snapshot.legacy,
            initialized: false,
            last_hash: snapshot.hash,
            generation: 0,
            session: 0,
            clock,
        };
        let _ = desktop.refresh(config.max_text_bytes.min(crate::protocol::MAX_TEXT_BYTES));
        let mesh = crate::mesh::MeshCoordinator::new(peers.clone());
        Arc::new(Self {
            mesh,
            desktop: Mutex::new(desktop),
            config: Mutex::new(config),
            config_path,
            peers,
            stop: Arc::new(AtomicBool::new(false)),
            host: Mutex::new(None),
        })
    }

    pub fn configure_host(&self, port: u16, fingerprint: String) {
        *self.host.lock().unwrap() = Some((port, fingerprint));
    }
    pub fn registration(&self, token: String) -> Option<Value> {
        let (port, fingerprint) = self.host.lock().unwrap().clone()?;
        Some(
            json!({"v":2,"id":self.settings().server_id,"name":"CyIME Wayclip","port":port,"fingerprint":fingerprint,"token":token}),
        )
    }
    pub fn settings(&self) -> Config {
        // Keep reload and outbound policy application serialized. Otherwise a slow
        // request with an old "enabled" snapshot could undo a newer pause.
        let mut current = self.config.lock().unwrap();
        let mut effective = current.clone();
        if let Some(path) = &self.config_path {
            match std::fs::read_to_string(path)
                .ok()
                .and_then(|s| Config::updated_from_toml(&s, &current).ok())
            {
                Some(mut next) => {
                    next.server_id.clone_from(&current.server_id);
                    *current = next.clone();
                    effective = next;
                }
                None => {
                    effective.send_enabled = false;
                    effective.receive_enabled = false;
                    effective.allow_introductions = false;
                }
            }
        }
        self.peers.set_enabled(effective.send_enabled);
        self.mesh
            .set_enabled(effective.allow_introductions && effective.send_enabled);
        effective
    }
    pub fn get(&self) -> Result<Option<Value>, ClipboardRequestError> {
        let config = self.settings();
        if !config.send_enabled {
            return Err(error(403, "sync direction paused"));
        }
        let mut desktop = self.desktop.lock().unwrap();
        let changed = desktop
            .refresh(config.max_text_bytes.min(crate::protocol::MAX_TEXT_BYTES))
            .map_err(|_| error(503, "desktop clipboard unavailable"))?;
        let value = desktop.value.clone().filter(|_| desktop.stamp > 0);
        drop(desktop);
        if changed {
            if let Some(v) = &value {
                self.peers.publish(v.clone());
            }
        }
        Ok(value)
    }
    pub fn put(&self, body: &[u8], owner: &str) -> Result<Value, ClipboardRequestError> {
        let config = self.settings();
        if !config.receive_enabled {
            return Err(error(403, "sync direction paused"));
        }
        let incoming = parse_clipboard_body(body)?;
        if incoming["size"].as_u64().unwrap_or(0)
            > config.max_text_bytes.min(crate::protocol::MAX_TEXT_BYTES) as u64
        {
            return Err(error(413, "clipboard text exceeds configured limit"));
        }
        if incoming["text"].as_str().unwrap().trim().is_empty() {
            return Err(error(400, "clipboard text is blank"));
        }
        let numeric = incoming["stamp"].as_i64().filter(|n| *n > 0);
        if numeric.is_some_and(|n| n > now_ms().saturating_add(300_000)) {
            return Err(error(400, "clipboard timestamp is too far in the future"));
        }
        let mut desktop = self.desktop.lock().unwrap();
        if let Some(id) = incoming["stamp"]["device_id"].as_str() {
            if id.len() > 128 || (!desktop.legacy.contains_key(id) && desktop.legacy.len() >= 128) {
                return Err(error(413, "too many legacy clock identities"));
            }
        }
        let local_changed = desktop
            .refresh(config.max_text_bytes.min(crate::protocol::MAX_TEXT_BYTES))
            .map_err(|_| error(503, "desktop clipboard unavailable"))?;
        let old = desktop.value.clone();
        if local_changed && config.send_enabled {
            if let Some(v) = old {
                self.peers.publish(v);
            }
        }
        let duplicate = desktop
            .value
            .as_ref()
            .is_some_and(|v| v["hash"] == incoming["hash"]);
        let legacy = incoming["stamp"].as_object().map(|s| {
            (
                s["device_id"].as_str().unwrap().to_owned(),
                s["counter"].as_u64().unwrap(),
            )
        });
        let stale = numeric.is_some_and(|n| n <= desktop.stamp)
            || legacy
                .as_ref()
                .is_some_and(|(id, n)| desktop.legacy.get(id).is_some_and(|old| n <= old));
        // Like Android DeviceHost: acknowledge an old copy without overwriting a newer one.
        if stale && !duplicate {
            return Ok(incoming);
        }
        if !duplicate {
            let expected_generation = desktop.generation;
            (desktop.write)(incoming["text"].as_str().unwrap(), expected_generation).map_err(
                |e| {
                    eprintln!("desktop clipboard write failed: {e}");
                    error(503, "desktop clipboard unavailable")
                },
            )?;
        }
        if let Some((id, n)) = legacy {
            let counter = desktop.legacy.entry(id).or_default();
            *counter = (*counter).max(n);
        }
        desktop.stamp = numeric
            .unwrap_or_else(|| now_ms().max(desktop.stamp.saturating_add(1)))
            .max(desktop.stamp);
        let mut committed = incoming.clone();
        // Outgoing ordering always uses Android's millisecond clock, never a legacy counter.
        committed["stamp"] = json!(desktop.stamp);
        desktop.value = Some(committed.clone());
        desktop.last_hash = committed["hash"].as_str().map(str::to_owned);
        let snapshot = crate::clock_store::ClockSnapshot {
            hash: desktop.last_hash.clone(),
            stamp: desktop.stamp,
            legacy: desktop.legacy.clone(),
        };
        let persist = desktop.clock.save(snapshot);
        drop(desktop);
        persist.map_err(|e| {
            eprintln!("persist clipboard clock failed: {e}");
            error(503, "clipboard clock unavailable")
        })?;
        if config.send_enabled && !duplicate {
            self.peers.publish_except(committed, Some(owner));
        }
        Ok(incoming)
    }
    /// Runs clipboard observation until the runtime is dropped. `wait` blocks up
    /// to its timeout and returns whether a native selection event woke it, so
    /// copies propagate immediately while config and revocation maintenance stay
    /// bounded to a 500 ms cadence.
    pub fn start_monitor(
        self: &Arc<Self>,
        device_path: Option<PathBuf>,
        mut wait: Option<ClipboardEventWait>,
    ) {
        let weak = Arc::downgrade(self);
        let stop = Arc::clone(&self.stop);
        std::thread::spawn(move || {
            let mut next_maintenance = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                let maintain = now >= next_maintenance;
                if maintain {
                    next_maintenance = now + Duration::from_millis(500);
                }
                let Some(runtime) = weak.upgrade() else {
                    break;
                };
                if maintain {
                    if let Some(path) = &device_path {
                        let owners =
                            crate::protocol::devices::DeviceRegistry::open(Some(path.clone()))
                                .map(|registry| {
                                    registry
                                        .list()
                                        .into_iter()
                                        .map(|d| d.device_id)
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                        let _ = runtime.peers.retain_owners(&owners);
                    }
                }
                let config = runtime.settings();
                if config.send_enabled {
                    let _ = runtime.get();
                } else {
                    // Observe local changes even while sending is paused; never enqueue them.
                    let _ = runtime
                        .desktop
                        .lock()
                        .unwrap()
                        .refresh(config.max_text_bytes.min(crate::protocol::MAX_TEXT_BYTES));
                }
                drop(runtime);
                let timeout = next_maintenance.saturating_duration_since(Instant::now());
                match wait.as_mut() {
                    Some(wait_for_selection) => {
                        let _ = wait_for_selection(timeout);
                    }
                    None => std::thread::sleep(timeout),
                }
            }
        });
    }
}
impl Drop for SyncRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn profile(text: &str, stamp: i64) -> Value {
    let mut value = serde_json::to_value(
        crate::protocol::ClipboardProfile::text(text).expect("validated local text"),
    )
    .unwrap();
    value["stamp"] = json!(stamp);
    value["source"] = json!("cyime-wayland");
    value
}
fn error(status: u16, reason: &'static str) -> ClipboardRequestError {
    ClipboardRequestError { status, reason }
}

#[cfg(test)]
#[path = "sync_runtime_tests.rs"]
mod tests;
