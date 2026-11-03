use std::path::PathBuf;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub allow_insecure_v1: bool,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub max_text_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            allow_insecure_v1: false,
            send_enabled: true,
            receive_enabled: true,
            max_text_bytes: crate::protocol::MAX_TEXT_BYTES,
        }
    }
}
impl Config {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("cyime-wayclip").join("config.toml"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secure_defaults() {
        let c = Config::default();
        assert!(!c.allow_insecure_v1);
        assert!(c.send_enabled && c.receive_enabled);
    }
}
