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
    pub fn to_toml(&self) -> String {
        format!("allow_insecure_v1 = {}\nsend_enabled = {}\nreceive_enabled = {}\nmax_text_bytes = {}\n", self.allow_insecure_v1, self.send_enabled, self.receive_enabled, self.max_text_bytes)
    }
    pub fn from_toml(input: &str) -> Self {
        let mut c = Self::default();
        for line in input.lines() {
            let mut it = line.splitn(2, '=');
            let (Some(k), Some(v)) = (it.next(), it.next()) else {
                continue;
            };
            match k.trim() {
                "allow_insecure_v1" => c.allow_insecure_v1 = v.trim() == "true",
                "send_enabled" => c.send_enabled = v.trim() != "false",
                "receive_enabled" => c.receive_enabled = v.trim() != "false",
                "max_text_bytes" => c.max_text_bytes = v.trim().parse().unwrap_or(c.max_text_bytes),
                _ => {}
            }
        }
        c
    }
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
