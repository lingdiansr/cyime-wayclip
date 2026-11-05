#[cfg(not(feature = "wayland"))]
use cyime_wayclip::clipboard::{ClipboardProvider, UnavailableProvider};
use cyime_wayclip::config::Config;
mod terminal_qr;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let verbosity = args
        .iter()
        .skip(1)
        .filter(|a| matches!(a.as_str(), "-v" | "-vv" | "-vvv"))
        .map(|a| a.len() - 1)
        .max()
        .unwrap_or(0);
    cyime_wayclip::logging::set_verbosity(verbosity);
    if args
        .iter()
        .any(|a| a == "--show-content" || a == "--log-content")
    {
        cyime_wayclip::logging::set_show_content(true);
    }
    let command_index = args
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, a)| !a.starts_with('-') || matches!(a.as_str(), "--help" | "-h"))
        .map(|(i, _)| i);
    let command = command_index.and_then(|i| args.get(i)).map(String::as_str);
    let positional = command_index
        .and_then(|i| args.get(i + 1))
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str);
    match command {
        Some("doctor") => doctor(),
        Some("status") => status(),
        Some("run") | None => run(),
        Some("serve") => serve_v2(positional),
        Some("serve-v1") => serve(positional),
        Some("serve-v2") => serve_v2(positional),
        Some("pair") => pair(),
        Some("config") => config_command(&args[command_index.unwrap() + 1..]),
        Some("devices") => cli_result(devices_command(&args[command_index.unwrap() + 1..])),
        Some("nearby") => cli_result(nearby_command(&args[command_index.unwrap() + 1..])),
        Some("service") => service(positional),
        Some("help") | Some("--help") | Some("-h") => help(),
        Some(x) => {
            eprintln!("unknown command: {x}");
            std::process::exit(2)
        }
    }
}
fn help() {
    println!("cyime-wayclip - Wayland clipboard synchronization");
    println!("\nUSAGE:\n  cyime-wayclip <COMMAND> [ADDRESS]");
    println!("\nCOMMANDS:\n  run                  Inspect Wayland and run the user-session worker");
    println!("  doctor               Show config path and Wayland capability");
    println!("  status               Alias for doctor");
    println!("  config init          Create the default config file");
    println!("  config set KEY VALUE Set a synchronization option atomically");
    println!("  devices list         List paired devices (never prints tokens)");
    println!("  devices revoke ID    Revoke a device and delegated credentials");
    println!("  nearby list          Show pending requests and comparison codes");
    println!("  nearby approve ID    Approve after comparing the phone code");
    println!("  nearby reject ID     Reject a pending request");
    println!("  serve-v1 [ADDRESS]   Start the explicitly enabled legacy HTTP server");
    println!("  pair                 Print a v1 pairing code and origin");
    println!("  serve [ADDRESS]      Alias for serve-v2");
    println!("  serve-v2 [ADDRESS]   Start the HTTPS QR pairing server");
    println!("  service install      Install the systemd user service");
    println!("  service uninstall    Remove the systemd user service");
    println!("  service status       Show systemd user service status");
    println!("  service start|stop   Start or stop the user service");
}

fn service(action: Option<&str>) {
    let Some(config_dir) = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
    else {
        eprintln!("cannot determine XDG config directory");
        std::process::exit(1);
    };
    let unit_dir = config_dir.join("systemd/user");
    let unit_path = unit_dir.join("cyime-wayclip.service");
    let action = action.unwrap_or("status");
    match action {
        "install" => {
            if let Err(e) = std::fs::create_dir_all(&unit_dir).and_then(|_| {
                let exe = std::env::current_exe()?.display().to_string();
                let body = format!("[Unit]\nDescription=CyIME Wayclip clipboard synchronization\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart={exe} run\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=yes\n\n[Install]\nWantedBy=graphical-session.target\n");
                std::fs::write(&unit_path, body)
            }) { eprintln!("install service: {e}"); std::process::exit(1); }
            println!("installed {}", unit_path.display());
            let _ = systemctl_user(&["daemon-reload"]);
            let _ = systemctl_user(&["enable", "--now", "cyime-wayclip.service"]);
        }
        "uninstall" => {
            let _ = systemctl_user(&["disable", "--now", "cyime-wayclip.service"]);
            if let Err(e) = std::fs::remove_file(&unit_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("remove service: {e}");
                }
            }
            let _ = systemctl_user(&["daemon-reload"]);
            println!("removed {}", unit_path.display());
        }
        "start" => {
            std::process::exit(systemctl_user(&["start", "cyime-wayclip.service"]));
        }
        "stop" => {
            std::process::exit(systemctl_user(&["stop", "cyime-wayclip.service"]));
        }
        "restart" => {
            std::process::exit(systemctl_user(&["restart", "cyime-wayclip.service"]));
        }
        "status" => {
            std::process::exit(systemctl_user(&["status", "cyime-wayclip.service"]));
        }
        _ => {
            eprintln!("usage: service install|uninstall|start|stop|restart|status");
            std::process::exit(2);
        }
    }
}
fn systemctl_user(args: &[&str]) -> i32 {
    match std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
    {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("systemctl --user: {e}");
            1
        }
    }
}

fn config_init() {
    let Some(path) = Config::path() else {
        eprintln!("cannot determine XDG config path");
        std::process::exit(1);
    };
    if path.exists() {
        println!("config already exists: {}", path.display());
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("create config directory: {e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = std::fs::write(&path, Config::default().to_toml()) {
        eprintln!("write config: {e}");
        std::process::exit(1);
    }
    println!("created config: {}", path.display());
}

fn doctor() {
    println!("config={:?}", Config::path());
    #[cfg(feature = "wayland")]
    {
        let c = cyime_wayclip::clipboard::wayland::detect();
        println!("wayland={:?}\n{}", c.provider_status(), c.diagnostic());
    }
    #[cfg(not(feature = "wayland"))]
    {
        let p = UnavailableProvider;
        println!("wayland={:?}\nfeature wayland disabled", p.status());
    }
}
fn status() {
    doctor();
}
fn run() {
    #[cfg(feature = "wayland")]
    {
        let c = cyime_wayclip::clipboard::wayland::detect();
        println!("{}", c.diagnostic());
        if std::env::var("CYIME_VERBOSITY").is_ok() {
            eprintln!("Wayland globals: {:?}", c.globals);
        }
        if c.provider_status() == cyime_wayclip::clipboard::ProviderStatus::Ready {
            let addr =
                std::env::var("CYIME_WAYCLIP_BIND").unwrap_or_else(|_| "0.0.0.0:9876".into());
            serve_v2(Some(&addr));
        }
    }
    #[cfg(not(feature = "wayland"))]
    println!("cyime-wayclip: no Wayland provider available (build with --features wayland)");
}

fn serve(bind: Option<&str>) {
    #[cfg(not(feature = "wayland"))]
    let _ = bind;
    #[cfg(feature = "wayland")]
    {
        let config = Config::load().unwrap_or_else(|e| {
            eprintln!("load config: {e}");
            Config::default()
        });
        let mut s = cyime_wayclip::server::Server::<()>::new(config);
        s.mark_ready();
        let code = cyime_wayclip::protocol::generate_pairing_code();
        let uri = format!("http://{}", bind.unwrap_or("127.0.0.1:8765"));
        println!("pairing_code={code}\nv1_uri={uri}");
        terminal_qr::print(&uri);
        if let Err(e) =
            cyime_wayclip::http_server::serve_v1(bind.unwrap_or("127.0.0.1:8765"), s, &code)
        {
            eprintln!("serve error: {e}");
        }
    }
    #[cfg(not(feature = "wayland"))]
    {
        eprintln!("serve requires --features wayland");
    }
}

fn pair() {
    let code = cyime_wayclip::protocol::generate_pairing_code();
    println!("pairing_code={code}");
    let uri = "http://127.0.0.1:8765";
    println!("v1_uri={uri} (enter this origin in the Android Lua plugin)");
    terminal_qr::print(uri);
}

fn serve_v2(bind: Option<&str>) {
    let addr = bind.unwrap_or("0.0.0.0:9876");
    let bind_address: std::net::SocketAddr = addr.parse().unwrap_or_else(|e| {
        eprintln!("invalid bind address: {e}");
        std::process::exit(1);
    });
    let hosts = if bind_address.ip().is_unspecified() {
        discover_lan_hosts()
            .into_iter()
            .filter(|h| {
                h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_ipv4() == bind_address.is_ipv4())
            })
            .collect::<Vec<_>>()
    } else {
        vec![bind_address.ip().to_string()]
    };
    if bind_address.port() == 0
        || hosts.is_empty()
        || hosts.iter().all(|h| {
            h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        })
    {
        eprintln!("serve-v2 requires a reachable LAN host and nonzero port");
        std::process::exit(1);
    }
    let mut config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("load config: {e}");
            std::process::exit(1);
        }
    };
    let state_dir = match cyime_wayclip::config::state_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("state directory: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = cyime_wayclip::config::ensure_private_directory(&state_dir) {
        eprintln!("state directory: {e}");
        std::process::exit(1);
    }
    // Preserve an existing configured identity; generate and persist once for fresh installs.
    if !Config::path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|s| {
            s.lines().any(|l| {
                l.split_once('=')
                    .is_some_and(|(k, _)| k.trim() == "server_id")
            })
        })
    {
        config.server_id = match cyime_wayclip::tls::stable_server_id() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("server identity: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = config.save() {
            eprintln!("persist config: {e}");
            std::process::exit(1);
        }
    }
    match cyime_wayclip::tls::load_or_generate_identity("localhost", &state_dir) {
        Ok(id) => {
            let server_id = config.server_id.clone();
            let mut secret_bytes = [0u8; 32];
            if let Err(e) = getrandom::fill(&mut secret_bytes) {
                eprintln!("pairing entropy: {e}");
                std::process::exit(1);
            }
            let secret = cyime_wayclip::protocol::encode_secret(&secret_bytes).unwrap();
            let offer = cyime_wayclip::protocol::PairOffer {
                v: 2,
                server_id: server_id.clone(),
                name: "CyIME Wayclip".into(),
                certificate_sha256: id.fingerprint.clone(),
                pairing_id: uuid::Uuid::new_v4().to_string(),
                secret: secret.clone(),
                endpoints: hosts
                    .iter()
                    .take(8)
                    .map(|h| {
                        format!(
                            "https://{}",
                            std::net::SocketAddr::new(
                                h.parse().expect("validated IP"),
                                bind_address.port()
                            )
                        )
                    })
                    .collect(),
                expires_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 300,
            };
            let uri = cyime_wayclip::protocol::encode_pair_uri(&offer).unwrap();
            println!("v2_uri={uri}");
            terminal_qr::print(&uri);
            let mut p = cyime_wayclip::protocol::PairingService::new(server_id, "CyIME Wayclip");
            p.set_offer(offer);
            if let Err(e) = cyime_wayclip::http_server::serve_v2_tls(
                addr,
                cyime_wayclip::server::Server::new(config),
                &id,
                &mut p,
            ) {
                eprintln!("serve-v2 error: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("TLS identity error: {e}");
            std::process::exit(1);
        }
    }
}

fn discover_lan_hosts() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for a in addrs {
            let ip = a.ip();
            if ip.is_loopback() {
                continue;
            }
            let ok = match ip {
                std::net::IpAddr::V4(v) => v.is_private() || v.is_link_local(),
                // QR endpoint scope IDs are not portable across devices. Use ULA
                // addresses; mDNS can resolve scoped link-local addresses separately.
                std::net::IpAddr::V6(v) => v.is_unique_local(),
            };
            if ok && !out.contains(&ip.to_string()) {
                // Prefer common Android/host hotspot subnets (10.42.0.0/24,
                // 192.168.43.0/24, 192.168.137.0/24) before other LAN IPs.
                let text = ip.to_string();
                if text.starts_with("10.42.0.")
                    || text.starts_with("192.168.43.")
                    || text.starts_with("192.168.137.")
                {
                    out.insert(0, text);
                } else {
                    out.push(text);
                }
            }
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}

fn cli_result(result: std::io::Result<()>) {
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn cli_invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}
fn config_command(args: &[String]) {
    if args.first().map(String::as_str) == Some("init") {
        config_init();
        return;
    }
    cli_result((|| {
        if args.len() != 3 || args[0] != "set" {
            return Err(cli_invalid("usage: config init | config set KEY VALUE"));
        }
        let mut c = Config::load()?;
        match args[1].as_str() {
            "send_enabled" | "receive_enabled" | "nearby_enabled" | "allow_introductions" => {
                let value = match args[2].as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(cli_invalid("value must be true or false")),
                };
                match args[1].as_str() {
                    "send_enabled" => c.send_enabled = value,
                    "receive_enabled" => c.receive_enabled = value,
                    "nearby_enabled" => c.nearby_enabled = value,
                    _ => c.allow_introductions = value,
                }
            }
            "max_text_bytes" => {
                c.max_text_bytes = args[2]
                    .parse()
                    .map_err(|_| cli_invalid("invalid byte limit"))?;
                if c.max_text_bytes == 0
                    || c.max_text_bytes > cyime_wayclip::protocol::MAX_TEXT_BYTES
                {
                    return Err(cli_invalid("byte limit must be 1..=65536"));
                }
            }
            _ => return Err(cli_invalid("unknown configuration key")),
        }
        c.save()?;
        println!(
            "updated {} (nearby advertisement changes require restart)",
            args[1]
        );
        Ok(())
    })());
}
fn devices_command(args: &[String]) -> std::io::Result<()> {
    use cyime_wayclip::protocol::devices::DeviceRegistry;
    let dir = cyime_wayclip::config::state_dir()?;
    cyime_wayclip::config::ensure_private_directory(&dir)?;
    let mut devices = DeviceRegistry::open(Some(dir.join("devices.json")))?;
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" if args.len() <= 1 => {
            for d in devices.list() {
                println!("{}\t{}\t{}", d.device_id, d.name, d.client_id);
            }
        }
        "revoke" if args.len() == 2 => {
            if !devices.revoke(&args[1])? {
                return Err(cli_invalid("unknown device ID"));
            }
            // Persisted credential revocation is authoritative; the live watcher cancels
            // callbacks too, even when this best-effort peer-store cleanup fails.
            match cyime_wayclip::peers::PeerManager::open(Some(dir.join("peers.json"))) {
                Ok(p) => {
                    if let Err(e) = p.remove_owner(&args[1]) {
                        eprintln!("callback cleanup pending: {e}");
                    }
                }
                Err(e) => eprintln!("callback cleanup pending: {e}"),
            }
            println!("revoked {}", args[1]);
        }
        _ => return Err(cli_invalid("usage: devices list | devices revoke ID")),
    }
    Ok(())
}
fn nearby_command(args: &[String]) -> std::io::Result<()> {
    let dir = cyime_wayclip::discovery::default_directory();
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" if args.len() <= 1 => {
            for p in cyime_wayclip::discovery::pending(&dir)? {
                println!("{}\t{}\t{}\t{}", p.request_id, p.code, p.name, p.source);
            }
        }
        "approve" | "reject" if args.len() == 2 => {
            cyime_wayclip::discovery::decide(&dir, &args[1], args[0] == "approve")?;
            println!("{} {}", args[0], args[1]);
        }
        _ => {
            return Err(cli_invalid(
                "usage: nearby list | nearby approve ID | nearby reject ID",
            ))
        }
    }
    Ok(())
}
