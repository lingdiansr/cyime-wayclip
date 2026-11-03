use std::sync::atomic::{AtomicU8, Ordering};
static VERBOSITY: AtomicU8 = AtomicU8::new(0);
static SHOW_CONTENT: AtomicU8 = AtomicU8::new(0);
pub fn set_verbosity(level: usize) {
    VERBOSITY.store(level.min(3) as u8, Ordering::Relaxed);
}
pub fn set_show_content(enabled: bool) {
    SHOW_CONTENT.store(enabled as u8, Ordering::Relaxed);
}
pub fn show_content() -> bool {
    SHOW_CONTENT.load(Ordering::Relaxed) != 0
}
pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}
pub fn log(level: u8, message: impl std::fmt::Display) {
    if verbosity() >= level {
        eprintln!("[v{level}] {message}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caps_level() {
        set_verbosity(9);
        assert_eq!(verbosity(), 3);
    }
}
