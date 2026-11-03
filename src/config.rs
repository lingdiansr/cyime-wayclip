use std::{
    fs, io,
    path::{Path, PathBuf},
};
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
    pub fn load() -> io::Result<Self> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        match fs::read_to_string(path) {
            Ok(s) => Ok(Self::from_toml(&s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        Self::save_to(&path, self)
    }

    pub fn save_to(path: &Path, config: &Self) -> io::Result<()> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent")
        })?;
        fs::create_dir_all(dir)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, config.to_toml())?;
        fs::rename(tmp, path)
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
    #[test]
    fn round_trip_persistence_is_atomic() {
        let dir = std::env::temp_dir().join(format!("cyime-config-{}", std::process::id()));
        let path = dir.join("config.toml");
        let c = Config {
            allow_insecure_v1: true,
            send_enabled: false,
            receive_enabled: true,
            max_text_bytes: 42,
        };
        save_to_cleanup(&path, &c);
        assert_eq!(
            Config::from_toml(&std::fs::read_to_string(&path).unwrap()),
            c
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    fn save_to_cleanup(path: &std::path::Path, c: &Config) {
        Config::save_to(path, c).unwrap();
    }
}
