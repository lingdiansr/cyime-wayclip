use cyime_wayclip::{
    clipboard::{ClipboardProvider, UnavailableProvider},
    config::Config,
};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("doctor") => doctor(),
        Some("status") => status(),
        Some("run") | None => run(),
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
