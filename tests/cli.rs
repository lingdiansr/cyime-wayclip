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
            // Inherited image support must not put escape sequences in a pipe.
            .env("TERM_PROGRAM", "ghostty")
            .env("TERM", "xterm-ghostty")
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
fn pairing_qr_preserves_modules_and_quiet_zone_in_half_blocks() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["pair"]);
    let rows: Vec<Vec<char>> = output
        .lines()
        .skip(2)
        .map(|row| row.chars().collect())
        .collect();
    let qr = qrcodegen::QrCode::encode_text("http://127.0.0.1:8765", qrcodegen::QrCodeEcc::Medium)
        .unwrap();
    let border = 4;
    let width = (qr.size() + 2 * border) as usize;
    assert_eq!(rows.len(), width.div_ceil(2));
    for (y, row) in rows.iter().enumerate() {
        assert_eq!(row.len(), width);
        for (x, cell) in row.iter().enumerate() {
            let (top, bottom) = match cell {
                ' ' => (false, false),
                '▀' => (true, false),
                '▄' => (false, true),
                '█' => (true, true),
                _ => panic!("unexpected QR character: {cell:?}"),
            };
            for (dy, dark) in [(0, top), (1, bottom)] {
                assert_eq!(
                    dark,
                    qr.get_module(x as i32 - border, (2 * y + dy) as i32 - border),
                    "QR module at ({x}, {})",
                    2 * y + dy,
                );
            }
        }
    }
}

#[test]
fn kitty_terminal_receives_a_lossless_qr_image() {
    use base64::Engine;
    use std::{
        fs::File,
        io::{Cursor, Read},
        os::fd::FromRawFd,
        process::Stdio,
    };

    let mut master = -1;
    let mut slave = -1;
    // The child must see a real terminal, not a pipe with spoofed environment.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_cyime-wayclip"))
        .arg("pair")
        .env("TERM", "xterm-kitty")
        .env("TERM_PROGRAM", "kitty")
        .env_remove("TMUX")
        .env_remove("STY")
        .stdin(Stdio::null())
        .stdout(slave)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            // Linux reports the closed slave as EIO.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read terminal: {e}"),
        }
    }
    assert!(child.wait().unwrap().success());
    let output = String::from_utf8(output).unwrap();
    let mut encoded = String::new();
    for chunk in output.split("\x1b_G").skip(1) {
        let (header, payload) = chunk.split_once(';').unwrap();
        assert!(header.contains("m="));
        encoded.push_str(payload.split_once("\x1b\\").unwrap().0);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let mut decoder = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
    let mut pixels = vec![0; decoder.output_buffer_size().unwrap()];
    let info = decoder.next_frame(&mut pixels).unwrap();
    assert_eq!(info.color_type, png::ColorType::Grayscale);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert_eq!(info.width, info.height);
    let qr = qrcodegen::QrCode::encode_text("http://127.0.0.1:8765", qrcodegen::QrCodeEcc::Medium)
        .unwrap();
    let modules = qr.size() as u32 + 8;
    assert_eq!(info.width % modules, 0);
    let scale = info.width / modules;
    assert!(scale > 0);
    for y in 0..info.height {
        for x in 0..info.width {
            let dark = qr.get_module((x / scale) as i32 - 4, (y / scale) as i32 - 4);
            assert_eq!(
                pixels[(y * info.width + x) as usize],
                if dark { 0 } else { 255 },
                "QR pixel at ({x}, {y})"
            );
        }
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
