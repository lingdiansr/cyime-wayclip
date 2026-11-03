use cyime_wayclip::{
    clipboard::{ClipboardProvider, ProviderStatus, UnavailableProvider},
    config::Config,
};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("doctor") => doctor(),
        Some("status") => println!("provider=Unsupported"),
        Some("run") | None => println!("cyime-wayclip: no Wayland provider available"),
        Some(x) => {
            eprintln!("unknown command: {x}");
            std::process::exit(2)
        }
    }
}
fn doctor() {
    let mut p = UnavailableProvider;
    println!("config={:?}", Config::path());
    println!("wayland={:?}", p.status());
}
