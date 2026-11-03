//! Durable clipboard ordering metadata only. No clipboard text or credentials are persisted.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

const MAX_BYTES: usize = 65_536;

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClockSnapshot {
    pub hash: Option<String>,
    pub stamp: i64,
    pub legacy: HashMap<String, u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    v: u8,
    hash: Option<String>,
    stamp: i64,
    legacy: HashMap<String, u64>,
}

pub struct ClockStore {
    path: Option<PathBuf>,
    current: ClockSnapshot,
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions or memory arguments.
    unsafe { libc::geteuid() }
}
fn validate(snapshot: &ClockSnapshot) -> io::Result<()> {
    if snapshot.stamp < 0 {
        return Err(invalid("clock stamp must not be negative"));
    }
    if snapshot.hash.as_ref().is_some_and(|s| {
        s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(invalid(
            "clock hash must be 64 lowercase hexadecimal characters",
        ));
    }
    if snapshot.legacy.len() > 128 || snapshot.legacy.keys().any(|key| key.len() > 128) {
        return Err(invalid("legacy clock map exceeds bounds"));
    }
    Ok(())
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
fn check_path(path: &Path) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(invalid("clock path has no filename"));
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut prefix = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid("clock path must not contain parent traversal"));
        }
        prefix.push(component);
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(invalid("clock path contains a symlink"));
                }
                if prefix == absolute {
                    check_file_metadata(&meta)?;
                } else {
                    if !meta.is_dir() {
                        return Err(invalid("clock ancestor is not a directory"));
                    }
                    // Writable ancestors could permit a different user to swap the
                    // private directory. Root-owned sticky /tmp is the exception.
                    if (prefix != Path::new("/") && meta.uid() != uid() && meta.uid() != 0)
                        || (meta.mode() & 0o022 != 0
                            && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0))
                    {
                        return Err(invalid("unsafe clock ancestor"));
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && prefix == absolute => {}
            Err(e) => return Err(e),
        }
    }
    let directory = fs::symlink_metadata(parent(path))?;
    if !directory.is_dir()
        || directory.uid() != uid()
        || directory.permissions().mode() & 0o777 != 0o700
    {
        return Err(invalid(
            "clock parent must be owned by this user with mode 0700",
        ));
    }
    Ok(())
}
fn check_file_metadata(meta: &fs::Metadata) -> io::Result<()> {
    if !meta.is_file()
        || meta.uid() != uid()
        || meta.permissions().mode() & 0o777 != 0o600
        || meta.nlink() != 1
    {
        return Err(invalid(
            "clock store must be an owned regular mode 0600 file without hard links",
        ));
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err(invalid("clock store exceeds 64 KiB"));
    }
    Ok(())
}

impl ClockStore {
    /// The caller creates the private parent directory. Corrupt or unsafe files
    /// are errors, never silently replaced with an empty ordering history.
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let mut current = ClockSnapshot::default();
        if let Some(path) = &path {
            check_path(path)?;
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(target_os = "linux")]
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            match options.open(path) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    check_file_metadata(&metadata)?;
                    check_path(path)?;
                    let named = fs::symlink_metadata(path)?;
                    if metadata.dev() != named.dev() || metadata.ino() != named.ino() {
                        return Err(invalid("clock store changed while opening"));
                    }
                    let mut bytes = Vec::new();
                    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
                    if bytes.len() > MAX_BYTES {
                        return Err(invalid("clock store exceeds 64 KiB"));
                    }
                    let stored: Stored = serde_json::from_slice(&bytes)
                        .map_err(|_| invalid("invalid clock store JSON"))?;
                    if stored.v != 1 {
                        return Err(invalid("unsupported clock store schema"));
                    }
                    current = ClockSnapshot {
                        hash: stored.hash,
                        stamp: stored.stamp,
                        legacy: stored.legacy,
                    };
                    validate(&current)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Self { path, current })
    }

    pub fn snapshot(&self) -> ClockSnapshot {
        self.current.clone()
    }

    /// Call once per accepted copy, not on every monitor tick. A repeated identical
    /// snapshot is a no-op. Memory changes only after successful durable storage.
    pub fn save(&mut self, snapshot: ClockSnapshot) -> io::Result<()> {
        validate(&snapshot)?;
        let bytes = serde_json::to_vec(&Stored {
            v: 1,
            hash: snapshot.hash.clone(),
            stamp: snapshot.stamp,
            legacy: snapshot.legacy.clone(),
        })?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("clock store exceeds 64 KiB"));
        }
        if let Some(path) = &self.path {
            check_path(path)?;
        }
        if snapshot == self.current
            && self
                .path
                .as_ref()
                .is_none_or(|path| fs::symlink_metadata(path).is_ok())
        {
            return Ok(());
        }
        if let Some(path) = &self.path {
            let directory = parent(path);
            let temporary = directory.join(format!(".cyime-clock-{}.tmp", uuid::Uuid::new_v4()));
            let mut created = false;
            let result = (|| {
                let mut options = OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                #[cfg(target_os = "linux")]
                options.custom_flags(libc::O_NOFOLLOW);
                let mut file = options.open(&temporary)?;
                created = true;
                // A restrictive umask must not leave a file unreadable on restart.
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                check_path(path)?;
                fs::rename(&temporary, path)?;
                File::open(directory)?.sync_all()
            })();
            if result.is_err() && created {
                let _ = fs::remove_file(&temporary);
            }
            result?;
        }
        self.current = snapshot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cyime-clock-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("clock.json")
        }
        fn write(&self, bytes: &[u8]) {
            fs::write(self.path(), bytes).unwrap();
            fs::set_permissions(self.path(), fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn snapshot() -> ClockSnapshot {
        ClockSnapshot {
            hash: Some("ab".repeat(32)),
            stamp: 123456789,
            legacy: HashMap::from([("peer".into(), 7)]),
        }
    }
    #[test]
    fn restart_stable_hash_stamp_and_no_redundant_write() {
        let dir = Directory::new();
        let mut store = ClockStore::open(Some(dir.path())).unwrap();
        assert_eq!(store.snapshot(), ClockSnapshot::default());
        store.save(snapshot()).unwrap();
        let before = fs::metadata(dir.path()).unwrap();
        store.save(snapshot()).unwrap();
        let after = fs::metadata(dir.path()).unwrap();
        assert_eq!(
            (before.ino(), before.modified().unwrap()),
            (after.ino(), after.modified().unwrap())
        );
        assert_eq!(
            ClockStore::open(Some(dir.path())).unwrap().snapshot(),
            snapshot()
        );
        let bytes = fs::read_to_string(dir.path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json.as_object().unwrap().len(), 4);
        assert!(json.get("text").is_none() && json.get("secret").is_none());
    }
    #[test]
    fn corrupt_oversized_or_unknown_schema_fail_closed() {
        let dir = Directory::new();
        for data in [
            b"not JSON".to_vec(),
            vec![b' '; MAX_BYTES + 1],
            br#"{"v":2,"hash":null,"stamp":0,"legacy":{}}"#.to_vec(),
            br#"{"v":1,"hash":"BAD","stamp":0,"legacy":{}}"#.to_vec(),
            br#"{"v":1,"hash":null,"stamp":-1,"legacy":{}}"#.to_vec(),
            br#"{"v":1,"hash":null,"stamp":0,"legacy":{},"text":"private"}"#.to_vec(),
        ] {
            dir.write(&data);
            assert!(ClockStore::open(Some(dir.path())).is_err());
        }
    }
    #[test]
    fn unsafe_permissions_and_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let dir = Directory::new();
        let mut store = ClockStore::open(Some(dir.path())).unwrap();
        store.save(snapshot()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ClockStore::open(Some(dir.path())).is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ClockStore::open(Some(dir.path())).is_err());
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.0.join("link");
        symlink(dir.path(), &link).unwrap();
        assert!(ClockStore::open(Some(link)).is_err());
        let ancestor = dir.0.join("ancestor");
        symlink(&dir.0, &ancestor).unwrap();
        assert!(ClockStore::open(Some(ancestor.join("clock.json"))).is_err());
    }
    #[test]
    fn failed_save_does_not_commit_memory() {
        let dir = Directory::new();
        let mut store = ClockStore::open(Some(dir.path())).unwrap();
        store.save(snapshot()).unwrap();
        fs::remove_file(dir.path()).unwrap();
        fs::create_dir(dir.path()).unwrap();
        let mut next = snapshot();
        next.stamp += 1;
        assert!(store.save(next).is_err());
        assert_eq!(store.snapshot(), snapshot());
        let mut bad = snapshot();
        bad.hash = Some("AB".repeat(32));
        assert!(store.save(bad).is_err());
        assert_eq!(store.snapshot(), snapshot());
    }
    #[test]
    fn memory_mode_and_map_bounds() {
        let mut store = ClockStore::open(None).unwrap();
        store.save(snapshot()).unwrap();
        let mut bad = snapshot();
        bad.legacy.insert("é".repeat(65), 1);
        assert!(store.save(bad).is_err());
        let mut bad = snapshot();
        bad.legacy = (0..129).map(|i| (i.to_string(), i)).collect();
        assert!(store.save(bad).is_err());
        assert_eq!(store.snapshot(), snapshot());
        let mut large = snapshot();
        large.legacy = (0..128)
            .map(|i| (format!("{i:03}{}", "\u{1}".repeat(125)), 0))
            .collect();
        assert!(store.save(large).is_err());
        assert_eq!(store.snapshot(), snapshot());
    }
}
