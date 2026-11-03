#[cfg(not(feature = "wayland"))]
use cyime_wayclip::clipboard::{ClipboardProvider, UnavailableProvider};
use cyime_wayclip::config::Config;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("doctor") => doctor(),
        Some("status") => status(),
        Some("run") | None => run(),
        Some("serve") => serve(args.get(2).map(String::as_str)),
        Some("serve-v2") => serve_v2(args.get(2).map(String::as_str)),
        Some("pair") => pair(),
        Some(x) => {
            eprintln!("unknown command: {x}");
            std::process::exit(2)
        }
    }
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
        if c.provider_status() == cyime_wayclip::clipboard::ProviderStatus::Ready {
            match cyime_wayclip::clipboard::wayland::paste_text() {
                Ok(s) => println!("clipboard_bytes={}", s.len()),
                Err(e) => println!("read_error={e}"),
            }
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
        let mut s = cyime_wayclip::server::Server::<()>::new(Config::default());
        s.mark_ready();
        let code = cyime_wayclip::protocol::generate_pairing_code();
        let uri = format!("http://{}", bind.unwrap_or("127.0.0.1:8765"));
        println!("pairing_code={code}\nv1_uri={uri}");
        print_qr(&uri);
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

fn print_qr(text: &str) {
    if let Ok(q) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) {
        for y in 0..q.size() {
            println!(
                "{}",
                (0..q.size())
                    .map(|x| if q.get_module(x, y) { "██" } else { "  " })
                    .collect::<String>()
            );
        }
    }
}

fn pair() {
    let code = cyime_wayclip::protocol::generate_pairing_code();
    println!("pairing_code={code}");
    let uri = "http://127.0.0.1:8765";
    println!("v1_uri={uri} (enter this origin in the Android Lua plugin)");
    print_qr(uri);
}

fn serve_v2(bind: Option<&str>) {
    let addr = bind.unwrap_or("0.0.0.0:9876");
    let hosts = if addr.starts_with("0.0.0.0:") {
        discover_lan_hosts()
    } else {
        vec![addr.split(':').next().unwrap_or("").into()]
    };
    if hosts.is_empty() || hosts.iter().all(|h| h == "127.0.0.1" || h == "localhost") {
        eprintln!("serve-v2 requires a reachable LAN host");
        return;
    }
    match cyime_wayclip::tls::generate_identity("localhost") {
        Ok(id) => {
            let secret = cyime_wayclip::protocol::encode_secret(&[9; 32]).unwrap();
            let offer = cyime_wayclip::protocol::PairOffer {
                v: 2,
                server_id: "cyime-wayclip".into(),
                name: "CyIME Wayclip".into(),
                certificate_sha256: id.fingerprint.clone(),
                pairing_id: "pairing".into(),
                secret: secret.clone(),
                endpoints: hosts
                    .iter()
                    .take(8)
                    .map(|h| {
                        format!(
                            "https://{}:{}",
                            h,
                            addr.rsplit_once(':').map(|x| x.1).unwrap_or("9876")
                        )
                    })
                    .collect(),
                expires_at: 4102444800,
            };
            let uri = cyime_wayclip::protocol::encode_pair_uri(&offer).unwrap();
            println!("v2_uri={uri}");
            print_qr(&uri);
            let mut p =
                cyime_wayclip::protocol::PairingService::new("cyime-wayclip", "CyIME Wayclip");
            p.set_offer(offer);
            if let Err(e) = cyime_wayclip::http_server::serve_v2_tls(
                addr,
                cyime_wayclip::server::Server::new(Config::default()),
                &id,
                &mut p,
            ) {
                eprintln!("serve-v2 error: {e}")
            }
        }
        Err(e) => eprintln!("TLS identity error: {e}"),
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
                std::net::IpAddr::V6(v) => v.is_unicast_link_local(),
            };
            if ok && !out.contains(&ip.to_string()) {
                out.push(ip.to_string());
            }
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}
