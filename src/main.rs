#[cfg(not(feature = "wayland"))]
use cyime_wayclip::clipboard::UnavailableProvider;
use cyime_wayclip::config::Config;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("doctor") => doctor(),
        Some("status") => status(),
        Some("run") | None => run(),
        Some("serve") => serve(args.get(2).map(String::as_str)),
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

fn serve() {
    #[cfg(feature = "wayland")]
    {
        let mut s = cyime_wayclip::server::Server::<()>::new(Config::default());
        s.mark_ready();
        if let Err(e) = cyime_wayclip::http_server::serve_v1("127.0.0.1:8765", s) {
            eprintln!("serve error: {e}");
        }
    }
    #[cfg(not(feature = "wayland"))]
    {
        eprintln!("serve requires --features wayland");
    }
}
