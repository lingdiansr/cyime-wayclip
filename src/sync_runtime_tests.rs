use super::*;
use crate::peers::Peer;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

struct Fixture {
    runtime: Arc<SyncRuntime>,
    clipboard: Arc<Mutex<Option<String>>>,
    writes: Arc<AtomicUsize>,
    read_fails: Arc<AtomicBool>,
    write_fails: Arc<AtomicBool>,
}
impl Fixture {
    fn new(config: Config, path: Option<PathBuf>) -> Self {
        let clipboard = Arc::new(Mutex::new(None::<String>));
        let writes = Arc::new(AtomicUsize::new(0));
        let read_fails = Arc::new(AtomicBool::new(false));
        let write_fails = Arc::new(AtomicBool::new(false));
        let read_clipboard = clipboard.clone();
        let read_flag = read_fails.clone();
        let write_clipboard = clipboard.clone();
        let write_flag = write_fails.clone();
        let count = writes.clone();
        let runtime = SyncRuntime::new(
            config,
            path,
            PeerManager::open(None).unwrap(),
            Box::new(move || {
                if read_flag.load(Ordering::Relaxed) {
                    Err("injected read failure".into())
                } else {
                    Ok(read_clipboard.lock().unwrap().clone())
                }
            }),
            Box::new(move |text| {
                if write_flag.load(Ordering::Relaxed) {
                    return Err("injected write failure".into());
                }
                count.fetch_add(1, Ordering::Relaxed);
                *write_clipboard.lock().unwrap() = Some(text.into());
                Ok(())
            }),
        );
        Self {
            runtime,
            clipboard,
            writes,
            read_fails,
            write_fails,
        }
    }
    fn standard() -> Self {
        Self::new(Config::default(), None)
    }
    fn local(&self, text: Option<&str>) {
        *self.clipboard.lock().unwrap() = text.map(str::to_owned);
    }
    fn put(&self, text: &str, stamp: i64) -> Result<Value, ClipboardRequestError> {
        self.runtime.put(&body(text, json!(stamp)), "phone")
    }
    fn write_count(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }
}
fn body(text: &str, stamp: Value) -> Vec<u8> {
    let mut value =
        serde_json::to_value(crate::protocol::ClipboardProfile::text(text).unwrap()).unwrap();
    value["stamp"] = stamp;
    value["source"] = json!("CyIME-Android");
    serde_json::to_vec(&value).unwrap()
}
fn future() -> i64 {
    now_ms() + 10_000
}
fn legacy(counter: u64) -> Value {
    json!({"device_id":"legacy-phone", "counter":counter})
}

#[test]
fn android_stamp_is_stable_across_get_and_local_change_grows_it() {
    let f = Fixture::standard();
    let stamp = future();
    f.put("手机\r\n🌍", stamp).unwrap();
    for _ in 0..3 {
        let value = f.runtime.get().unwrap().unwrap();
        assert_eq!(value["stamp"], stamp);
        assert_eq!(value["text"], "手机\r\n🌍");
    }
    f.local(Some("new desktop copy"));
    let value = f.runtime.get().unwrap().unwrap();
    assert!(value["stamp"].as_i64().unwrap() > stamp);
    assert_eq!(f.runtime.get().unwrap().unwrap()["stamp"], value["stamp"]);
}

#[test]
fn old_numeric_stamp_never_overwrites_newer_desktop() {
    let f = Fixture::standard();
    let stamp = future();
    f.put("new", stamp).unwrap();
    f.put("old", stamp - 1).unwrap();
    f.put("equal stamp different content", stamp).unwrap();
    assert_eq!(f.write_count(), 1);
    assert_eq!(f.runtime.get().unwrap().unwrap()["text"], "new");
}

#[test]
fn identical_content_is_deduplicated_even_with_new_timestamp() {
    let f = Fixture::standard();
    let stamp = future();
    f.put("same", stamp).unwrap();
    f.put("same", stamp).unwrap();
    f.put("same", stamp + 1).unwrap();
    assert_eq!(f.write_count(), 1);
    assert_eq!(f.runtime.get().unwrap().unwrap()["stamp"], stamp + 1);
}

#[test]
fn legacy_counter_never_regresses_on_duplicate_content() {
    let f = Fixture::standard();
    f.runtime.put(&body("new", legacy(10)), "phone").unwrap();
    f.runtime.put(&body("new", legacy(2)), "phone").unwrap();
    f.runtime.put(&body("stale", legacy(5)), "phone").unwrap();
    assert_eq!(
        f.write_count(),
        1,
        "duplicate old counter must not reopen stale writes"
    );
    assert_eq!(f.runtime.get().unwrap().unwrap()["text"], "new");
    assert_eq!(f.runtime.desktop.lock().unwrap().legacy["legacy-phone"], 10);
}

#[test]
fn desktop_read_failure_is_503_and_never_serves_cached_success() {
    let f = Fixture::standard();
    f.put("cached", future()).unwrap();
    let old = f.runtime.desktop.lock().unwrap().value.clone();
    f.read_fails.store(true, Ordering::Relaxed);
    assert_eq!(f.runtime.get().unwrap_err().status, 503);
    assert_eq!(f.put("not applied", future() + 1).unwrap_err().status, 503);
    assert_eq!(f.write_count(), 1);
    assert_eq!(f.runtime.desktop.lock().unwrap().value, old);
}

#[test]
fn desktop_write_failure_is_503_without_cache_or_clock_commit() {
    let f = Fixture::standard();
    let stamp = future();
    f.put("good", stamp).unwrap();
    let old = f.runtime.desktop.lock().unwrap().value.clone();
    f.write_fails.store(true, Ordering::Relaxed);
    assert_eq!(f.put("bad", stamp + 1).unwrap_err().status, 503);
    let desktop = f.runtime.desktop.lock().unwrap();
    assert_eq!(desktop.value, old);
    assert_eq!(desktop.stamp, stamp);
    drop(desktop);
    assert_eq!(f.write_count(), 1);
    assert_eq!(f.runtime.get().unwrap().unwrap()["text"], "good");
}

#[test]
fn paused_directions_are_403_and_config_reload_is_immediate() {
    let directory =
        std::env::temp_dir().join(format!("wayclip-runtime-config-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("config.toml");
    let mut config = Config {
        send_enabled: false,
        receive_enabled: false,
        ..Config::default()
    };
    Config::save_to(&path, &config).unwrap();
    let f = Fixture::new(Config::default(), Some(path.clone()));
    assert_eq!(f.runtime.get().unwrap_err().status, 403);
    assert_eq!(f.put("phone", future()).unwrap_err().status, 403);
    config.receive_enabled = true;
    Config::save_to(&path, &config).unwrap();
    f.put("phone", future()).unwrap();
    assert_eq!(f.runtime.get().unwrap_err().status, 403);
    config.send_enabled = true;
    Config::save_to(&path, &config).unwrap();
    assert_eq!(f.runtime.get().unwrap().unwrap()["text"], "phone");
    config.max_text_bytes = 2;
    Config::save_to(&path, &config).unwrap();
    assert_eq!(f.put("手机", future() + 1).unwrap_err().status, 413);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_input_never_writes_desktop() {
    let f = Fixture::standard();
    for text in ["", " \r\n\t"] {
        assert_eq!(f.put(text, future()).unwrap_err().status, 400);
    }
    let mut nul: Value = serde_json::from_slice(&body("valid", json!(future()))).unwrap();
    nul["text"] = json!("a\0b");
    assert_eq!(
        f.runtime
            .put(&serde_json::to_vec(&nul).unwrap(), "phone")
            .unwrap_err()
            .status,
        400
    );
    let mut oversized = nul;
    oversized["text"] = json!("x".repeat(crate::protocol::MAX_TEXT_BYTES + 1));
    assert_eq!(
        f.runtime
            .put(&serde_json::to_vec(&oversized).unwrap(), "phone")
            .unwrap_err()
            .status,
        413
    );
    assert_eq!(f.runtime.put(b"{", "phone").unwrap_err().status, 400);
    assert_eq!(f.write_count(), 0);
    assert!(f.runtime.get().unwrap().is_none());
}

#[test]
fn desktop_clear_and_unusable_text_return_no_content_not_old_cache() {
    let f = Fixture::standard();
    let mut stamp = future();
    for local in [None, Some(""), Some("  \n"), Some("a\0b")] {
        stamp += 10;
        f.put("cached", stamp).unwrap();
        f.local(local);
        assert!(
            f.runtime.get().unwrap().is_none(),
            "empty desktop must map to HTTP 204"
        );
        assert!(f.runtime.desktop.lock().unwrap().value.is_none());
    }
    f.put("cached", stamp + 1).unwrap();
    f.local(Some(&"x".repeat(crate::protocol::MAX_TEXT_BYTES + 1)));
    assert!(f.runtime.get().unwrap().is_none());
}

#[test]
fn asynchronous_fanout_excludes_source_owner_and_duplicate_content() {
    // Bound loopback port is deliberately not listening: attempted pushes fail immediately.
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let peer = |id: String| Peer {
        id,
        name: "test".into(),
        origin: format!("https://{addr}"),
        fingerprint: "ab".repeat(32),
        token: crate::protocol::encode_secret(&[9; 32]).unwrap(),
    };
    let f = Fixture::standard();
    f.runtime
        .peers
        .register("phone", peer(uuid::Uuid::new_v4().to_string()))
        .unwrap();
    f.runtime
        .peers
        .register("other", peer(uuid::Uuid::new_v4().to_string()))
        .unwrap();
    let start = Instant::now();
    f.put("fan out", future()).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "PUT must not wait for outbound network retries"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !f.runtime.peers.failed("other") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        f.runtime.peers.failed("other"),
        "other peer must receive a delivery attempt"
    );
    assert!(
        !f.runtime.peers.failed("phone"),
        "source owner must not get an echo"
    );
    // A duplicate received under the other owner's credentials must not fan out to the first.
    f.runtime
        .put(&body("fan out", json!(future() + 1)), "other")
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!f.runtime.peers.failed("phone"));
    assert!(f.runtime.peers.back("phone").is_some());
    assert_eq!(f.write_count(), 1);
}

// These fixtures seed the desktop BEFORE constructing the runtime: setting a
// selection after construction would test a new copy, not a restart baseline.
impl Fixture {
    fn seeded(
        config: Config,
        config_path: Option<PathBuf>,
        initial: Option<&str>,
        clock_path: Option<PathBuf>,
    ) -> Self {
        let clipboard = Arc::new(Mutex::new(initial.map(str::to_owned)));
        let writes = Arc::new(AtomicUsize::new(0));
        let read_fails = Arc::new(AtomicBool::new(false));
        let write_fails = Arc::new(AtomicBool::new(false));
        let read_clipboard = clipboard.clone();
        let read_flag = read_fails.clone();
        let write_clipboard = clipboard.clone();
        let write_flag = write_fails.clone();
        let count = writes.clone();
        let read: ClipboardRead = Box::new(move || {
            if read_flag.load(Ordering::Relaxed) {
                Err("injected read failure".into())
            } else {
                Ok(read_clipboard.lock().unwrap().clone())
            }
        });
        let write: ClipboardWrite = Box::new(move |text| {
            if write_flag.load(Ordering::Relaxed) {
                return Err("injected write failure".into());
            }
            count.fetch_add(1, Ordering::Relaxed);
            *write_clipboard.lock().unwrap() = Some(text.into());
            Ok(())
        });
        let peers = PeerManager::open(None).unwrap();
        let runtime = match clock_path {
            Some(path) => {
                SyncRuntime::persistent(config, config_path, peers, read, write, path).unwrap()
            }
            None => SyncRuntime::new(config, config_path, peers, read, write),
        };
        Self {
            runtime,
            clipboard,
            writes,
            read_fails,
            write_fails,
        }
    }
}
struct RegressionDirectory(PathBuf);
impl RegressionDirectory {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("cyime-order-regression-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(directory)
    }
    fn clock(&self) -> PathBuf {
        self.0.join("clock.json")
    }
    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }
}
impl Drop for RegressionDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn preexisting_selection_is_unknown_not_a_fake_new_copy() {
    let f = Fixture::seeded(Config::default(), None, Some("old desktop selection"), None);
    assert!(f.runtime.get().unwrap().is_none());
    assert_eq!(f.runtime.desktop.lock().unwrap().stamp, 0);
    let phone_stamp = now_ms() - 1_000;
    f.put("newer phone selection", phone_stamp).unwrap();
    assert_eq!(f.write_count(), 1);
    assert_eq!(
        f.clipboard.lock().unwrap().as_deref(),
        Some("newer phone selection")
    );
    assert_eq!(f.runtime.get().unwrap().unwrap()["stamp"], phone_stamp);
}

#[test]
fn zero_stamp_is_a_new_legacy_copy_not_an_old_numeric_stamp() {
    let f = Fixture::standard();
    let previous = future();
    f.put("first", previous).unwrap();
    f.put("zero stamp legacy Android", 0).unwrap();
    assert_eq!(f.write_count(), 2);
    let value = f.runtime.get().unwrap().unwrap();
    assert_eq!(value["text"], "zero stamp legacy Android");
    assert!(value["stamp"].as_i64().unwrap() > previous);
}

#[test]
fn unreasonable_future_stamps_fail_before_read_publish_or_clock_mutation() {
    let f = Fixture::standard();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    f.runtime
        .peers
        .register(
            "observer",
            Peer {
                id: uuid::Uuid::new_v4().to_string(),
                name: "observer".into(),
                origin: format!("https://{address}"),
                fingerprint: "ab".repeat(32),
                token: crate::protocol::encode_secret(&[8; 32]).unwrap(),
            },
        )
        .unwrap();
    let before = f.runtime.desktop.lock().unwrap().clock.snapshot();
    let cached = f.runtime.desktop.lock().unwrap().value.clone();
    f.local(Some("changed but not yet observed"));
    f.read_fails.store(true, Ordering::Relaxed);
    for stamp in [i64::MAX, now_ms() + 300_100] {
        assert_eq!(f.put("invalid future copy", stamp).unwrap_err().status, 400);
        let desktop = f.runtime.desktop.lock().unwrap();
        assert_eq!(desktop.clock.snapshot(), before);
        assert_eq!(desktop.value, cached);
        assert_eq!(desktop.stamp, before.stamp);
        assert_eq!(desktop.legacy, before.legacy);
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !f.runtime.peers.failed("observer"),
        "invalid upload must not enqueue local refresh fanout"
    );
    assert_eq!(f.write_count(), 0);
    assert_eq!(
        f.clipboard.lock().unwrap().as_deref(),
        Some("changed but not yet observed")
    );
    f.read_fails.store(false, Ordering::Relaxed);
    f.put("valid future-bounded copy", future()).unwrap();
    assert_eq!(f.write_count(), 1);
    assert_eq!(
        f.runtime.get().unwrap().unwrap()["text"],
        "valid future-bounded copy"
    );
}

#[test]
fn durable_same_hash_restart_preserves_numeric_and_legacy_ordering() {
    let directory = RegressionDirectory::new();
    let f = Fixture::seeded(Config::default(), None, None, Some(directory.clock()));
    f.put("saved copy", future()).unwrap();
    f.runtime
        .put(&body("saved copy", legacy(19)), "phone")
        .unwrap();
    let before = f.runtime.desktop.lock().unwrap().clock.snapshot();
    assert_eq!(before.legacy["legacy-phone"], 19);
    drop(f);
    let reopened = Fixture::seeded(
        Config::default(),
        None,
        Some("saved copy"),
        Some(directory.clock()),
    );
    assert_eq!(
        reopened.runtime.desktop.lock().unwrap().clock.snapshot(),
        before
    );
    assert_eq!(
        reopened.runtime.get().unwrap().unwrap()["stamp"],
        before.stamp
    );
    reopened.put("old numeric copy", before.stamp - 1).unwrap();
    reopened
        .runtime
        .put(&body("old legacy copy", legacy(18)), "phone")
        .unwrap();
    assert_eq!(reopened.write_count(), 0);
    assert_eq!(
        reopened.clipboard.lock().unwrap().as_deref(),
        Some("saved copy")
    );
    assert_eq!(
        reopened.runtime.desktop.lock().unwrap().clock.snapshot(),
        before
    );
    assert_eq!(
        crate::clock_store::ClockStore::open(Some(directory.clock()))
            .unwrap()
            .snapshot(),
        before
    );
}

#[test]
fn changed_hash_while_offline_is_unknown_until_a_new_copy() {
    let directory = RegressionDirectory::new();
    let f = Fixture::seeded(Config::default(), None, None, Some(directory.clock()));
    f.put("previous known selection", future()).unwrap();
    drop(f);
    let reopened = Fixture::seeded(
        Config::default(),
        None,
        Some("unobserved offline selection"),
        Some(directory.clock()),
    );
    assert!(reopened.runtime.get().unwrap().is_none());
    assert_eq!(reopened.runtime.desktop.lock().unwrap().stamp, 0);
    assert_eq!(reopened.write_count(), 0);
    let baseline = reopened.runtime.desktop.lock().unwrap().clock.snapshot();
    assert_eq!(
        baseline.hash.as_deref(),
        Some(
            crate::protocol::ClipboardProfile::text("unobserved offline selection")
                .unwrap()
                .hash
                .as_str()
        )
    );
    reopened.local(Some("real new desktop copy"));
    let value = reopened.runtime.get().unwrap().unwrap();
    assert_eq!(value["text"], "real new desktop copy");
    assert!(value["stamp"].as_i64().unwrap() > baseline.stamp);
    assert_eq!(
        reopened.runtime.get().unwrap().unwrap()["stamp"],
        value["stamp"]
    );
    assert_eq!(
        crate::clock_store::ClockStore::open(Some(directory.clock()))
            .unwrap()
            .snapshot()
            .stamp,
        value["stamp"].as_i64().unwrap()
    );
}

#[test]
fn invalid_or_unreadable_config_pauses_both_directions_without_identity_rotation() {
    let directory = RegressionDirectory::new();
    let path = directory.config();
    let config = Config::default();
    Config::save_to(&path, &config).unwrap();
    let f = Fixture::new(config.clone(), Some(path.clone()));
    for malformed in [
        "send_enabled=true\nreceive_enabled=garbage",
        "truncated setting",
        "max_text_bytes=0",
        "server_id=invalid",
    ] {
        std::fs::write(&path, malformed).unwrap();
        let settings = f.runtime.settings();
        assert!(!settings.send_enabled && !settings.receive_enabled);
        assert_eq!(settings.server_id, config.server_id);
        assert_eq!(f.runtime.get().unwrap_err().status, 403);
        assert_eq!(f.put("blocked", future()).unwrap_err().status, 403);
        assert_eq!(f.write_count(), 0);
    }
    // Missing file and a directory where a file is expected are reliably unreadable
    // even when tests happen to run as root (unlike chmod 000).
    std::fs::remove_file(&path).unwrap();
    for directory_instead in [false, true] {
        if directory_instead {
            std::fs::create_dir(&path).unwrap();
        }
        assert_eq!(f.runtime.get().unwrap_err().status, 403);
        assert_eq!(f.put("blocked", future()).unwrap_err().status, 403);
        assert_eq!(f.runtime.settings().server_id, config.server_id);
    }
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "send_enabled=false\nreceive_enabled=false").unwrap();
    assert!(!f.runtime.settings().send_enabled);
    for partial in [
        "",
        "# temporarily empty config\n",
        "max_text_bytes=512",
        "nearby_enabled=false\n",
        "server_id=\"00000000-0000-4000-8000-000000000007\"",
    ] {
        std::fs::write(&path, partial).unwrap();
        let settings = f.runtime.settings();
        assert!(!settings.send_enabled && !settings.receive_enabled);
        assert_eq!(settings.server_id, config.server_id);
        assert_eq!(f.runtime.get().unwrap_err().status, 403);
        assert_eq!(f.put("still blocked", future()).unwrap_err().status, 403);
    }
    std::fs::write(&path, "send_enabled=true\nreceive_enabled=true").unwrap();
    f.put("explicitly resumed", future()).unwrap();
    assert_eq!(f.write_count(), 1);
    assert_eq!(f.runtime.settings().server_id, config.server_id);
}
