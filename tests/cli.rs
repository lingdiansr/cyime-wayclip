//! Exercise the actual executable with isolated XDG paths, not the desktop service.
#![cfg(target_os = "linux")]
use cyime_wayclip::{
    config::Config,
    discovery::{Nearby, NearbyResult},
    protocol::{devices::DeviceRegistry, encode_secret},
};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cyime-cli-test-{}", uuid::Uuid::new_v4()));
        cyime_wayclip::config::ensure_private_directory(&root).unwrap();
        Self(root)
    }
    fn state(&self) -> PathBuf {
        self.0.join("state/cyime-wayclip")
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cyime-wayclip"))
            .args(args)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            // Deliberately unrelated: approvals must not depend on this directory.
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .output()
            .unwrap()
    }
    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn nearby_cli_reads_listener_state_directory_and_requires_local_decision() {
    let sandbox = Sandbox::new();
    let mut nearby = Nearby::open(Some(sandbox.state().join("nearby")), "ab".repeat(32)).unwrap();
    let body = json!({"v":2,"client_id":uuid::Uuid::new_v4().to_string(),"client_name":"CLI test phone","secret":encode_secret(&[19;32]).unwrap()});
    let source = "127.0.0.1".parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (id, code) = match nearby.request(&body, source, now).unwrap() {
        NearbyResult::Pending {
            request_id, code, ..
        } => (request_id, code),
        _ => panic!("not pending"),
    };
    let listed = sandbox.run(&["nearby", "list"]);
    assert!(listed.contains(&id) && listed.contains(&code) && listed.contains("CLI test phone"));
    assert!(!listed.contains(body["secret"].as_str().unwrap()));
    assert!(matches!(
        nearby.request(&body, source, now + 1).unwrap(),
        NearbyResult::Pending { .. }
    ));
    sandbox.run(&["nearby", "approve", &id]);
    assert!(matches!(
        nearby.request(&body, source, now + 2).unwrap(),
        NearbyResult::Approved { .. }
    ));
    nearby.complete(&id).unwrap();
    assert!(sandbox.run(&["nearby", "list"]).is_empty());
    assert!(!sandbox
        .command(&["nearby", "approve", "../bad"])
        .status
        .success());
}

#[test]
fn config_and_device_cli_are_durable_and_do_not_disclose_tokens() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "init"]);
    let path = sandbox.0.join("config/cyime-wayclip/config.toml");
    let initial =
        Config::updated_from_toml(&fs::read_to_string(&path).unwrap(), &Config::default()).unwrap();
    sandbox.run(&["config", "set", "send_enabled", "false"]);
    sandbox.run(&["config", "set", "allow_introductions", "true"]);
    assert!(!sandbox
        .command(&["config", "set", "send_enabled", "maybe"])
        .status
        .success());
    assert!(!sandbox
        .command(&["config", "set", "max_text_bytes", "65537"])
        .status
        .success());
    let changed =
        Config::updated_from_toml(&fs::read_to_string(&path).unwrap(), &Config::default()).unwrap();
    assert!(!changed.send_enabled && changed.allow_introductions);
    assert_eq!(initial.server_id, changed.server_id);
    cyime_wayclip::config::ensure_private_directory(&sandbox.state()).unwrap();
    let file = sandbox.state().join("devices.json");
    let mut registry = DeviceRegistry::open(Some(file.clone())).unwrap();
    let (id, token) = registry
        .issue(&uuid::Uuid::new_v4().to_string(), "CLI phone")
        .unwrap();
    let (_, grant) = registry
        .grant(&token, &uuid::Uuid::new_v4().to_string(), "delegated phone")
        .unwrap();
    let (_, other) = registry
        .issue(&uuid::Uuid::new_v4().to_string(), "other phone")
        .unwrap();
    let list = sandbox.run(&["devices", "list"]);
    assert!(list.contains(&id) && list.contains("CLI phone"));
    for secret in [&token, &grant, &other] {
        assert!(!list.contains(secret));
    }
    sandbox.run(&["devices", "revoke", &id]);
    let fresh = DeviceRegistry::open(Some(file)).unwrap();
    assert!(fresh.authenticate(&token).is_none() && fresh.authenticate(&grant).is_none());
    assert!(fresh.authenticate(&other).is_some());
    assert!(!sandbox
        .command(&["devices", "revoke", &id])
        .status
        .success());
}
