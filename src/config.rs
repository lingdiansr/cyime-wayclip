use std::{
    fs, io,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub allow_insecure_v1: bool,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub nearby_enabled: bool,
    pub allow_introductions: bool,
    pub max_text_bytes: usize,
    pub server_id: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            allow_insecure_v1: false,
            send_enabled: true,
            receive_enabled: true,
            nearby_enabled: true,
            allow_introductions: false,
            max_text_bytes: crate::protocol::MAX_TEXT_BYTES,
            server_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}
impl Config {
    pub fn to_toml(&self) -> String {
        format!("allow_insecure_v1 = {}\nsend_enabled = {}\nreceive_enabled = {}\nnearby_enabled = {}\nallow_introductions = {}\nmax_text_bytes = {}\nserver_id = \"{}\"\n", self.allow_insecure_v1, self.send_enabled, self.receive_enabled, self.nearby_enabled, self.allow_introductions, self.max_text_bytes, self.server_id)
    }
    /// Compatibility wrapper. Live reloads must use `updated_from_toml` so errors
    /// are observable and missing fields preserve the last trusted configuration.
    pub fn from_toml(input: &str) -> Self {
        let base = Self::default();
        Self::updated_from_toml(input, &base).unwrap_or(base)
    }

    /// Parse the supported flat TOML subset transactionally. Unspecified settings,
    /// especially paused directions and server identity, are inherited from `base`.
    pub fn updated_from_toml(input: &str, base: &Self) -> io::Result<Self> {
        let mut next = base.clone();
        for (index, raw) in input.lines().enumerate() {
            // Known string values are UUIDs, which cannot contain '#'. Unknown
            // keys are deliberately ignored, rather than interpreted as settings.
            let line = raw.split_once('#').map_or(raw, |(line, _)| line).trim();
            if line.is_empty() {
                continue;
            }
            let error = |reason: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid config line {}: {}", index + 1, reason),
                )
            };
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| error("expected key = value"))?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() {
                return Err(error("empty setting name"));
            }
            let boolean = || match value {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(error("expected true or false")),
            };
            match key {
                "allow_insecure_v1" => next.allow_insecure_v1 = boolean()?,
                "send_enabled" => next.send_enabled = boolean()?,
                "receive_enabled" => next.receive_enabled = boolean()?,
                "nearby_enabled" => next.nearby_enabled = boolean()?,
                "allow_introductions" => next.allow_introductions = boolean()?,
                "max_text_bytes" => {
                    let size = value
                        .parse::<usize>()
                        .map_err(|_| error("invalid max_text_bytes"))?;
                    if !(1..=65_536).contains(&size) {
                        return Err(error("max_text_bytes must be 1..65536"));
                    }
                    next.max_text_bytes = size;
                }
                "server_id" => {
                    let id = value
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                        .unwrap_or(value);
                    next.server_id = uuid::Uuid::parse_str(id)
                        .map_err(|_| error("server_id must be a UUID"))?
                        .to_string();
                }
                _ => {}
            }
        }
        Ok(next)
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
            Ok(s) => Self::updated_from_toml(&s, &Self::default()),
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
pub fn state_dir() -> io::Result<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|p| p.join("cyime-wayclip"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine state directory"))
}
pub fn ensure_private_directory(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "state directory path must not contain symlinks",
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "state path must be a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state directory must belong to current user",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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
            nearby_enabled: true,
            allow_introductions: false,
            max_text_bytes: 42,
            server_id: uuid::Uuid::new_v4().to_string(),
        };
        save_to_cleanup(&path, &c);
        assert_eq!(
            Config::from_toml(&std::fs::read_to_string(&path).unwrap()),
            c
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn partial_config_preserves_pauses_and_identity() {
        let base = Config {
            send_enabled: false,
            receive_enabled: false,
            nearby_enabled: false,
            max_text_bytes: 42,
            ..Config::default()
        };
        for input in [
            "",
            "# truncated to comments only",
            "allow_introductions = true",
            "unknown = ignored",
        ] {
            let merged = Config::updated_from_toml(input, &base).unwrap();
            assert!(!merged.send_enabled && !merged.receive_enabled && !merged.nearby_enabled);
            assert_eq!(merged.server_id, base.server_id);
            assert_eq!(merged.max_text_bytes, 42);
        }
    }
    #[test]
    fn malformed_config_is_rejected_transactionally() {
        let base = Config {
            send_enabled: false,
            ..Config::default()
        };
        let original = base.clone();
        for line in [
            "send_enabled =",
            "send_enabled = TRUE",
            "receive_enabled = 0",
            "nearby_enabled = yes",
            "allow_insecure_v1 = 1",
            "allow_introductions = nope",
            "max_text_bytes = 0",
            "max_text_bytes = 65537",
            "max_text_bytes = -1",
            "max_text_bytes = rubbish",
            "server_id = nope",
            "server_id = \"broken",
            "not an assignment",
            "=true",
        ] {
            let text = format!("send_enabled = true\n{line}\n");
            assert!(
                Config::updated_from_toml(&text, &base).is_err(),
                "accepted {line:?}"
            );
            assert_eq!(base, original);
        }
    }
    #[test]
    fn comments_unknown_keys_and_boundaries() {
        let base = Config::default();
        let id = uuid::Uuid::new_v4();
        let text = format!("# comment\n\n send_enabled = false # paused\nreceive_enabled = false # paused\nmax_text_bytes = 1 # lower bound\nserver_id = \"{id}\" # identity\nfuture_option = any-value # ignored\n");
        let merged = Config::updated_from_toml(&text, &base).unwrap();
        assert!(!merged.send_enabled && !merged.receive_enabled);
        assert_eq!(merged.server_id, id.to_string());
        assert_eq!(merged.max_text_bytes, 1);
        assert_eq!(
            Config::updated_from_toml("max_text_bytes = 65536", &base)
                .unwrap()
                .max_text_bytes,
            65_536
        );
        assert_eq!(
            Config::updated_from_toml(&base.to_toml(), &base).unwrap(),
            base
        );
    }
    fn save_to_cleanup(path: &std::path::Path, c: &Config) {
        Config::save_to(path, c).unwrap();
    }
}
