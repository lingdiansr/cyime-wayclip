//! Bounded, persistent device credentials. Raw tokens are returned once, never stored.
use super::encode_secret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_DEVICES: usize = 32;
const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_LABEL_BYTES: usize = 1024;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    pub device_id: String,
    pub client_id: String,
    pub name: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    summary: DeviceSummary,
    digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    v: u8,
    devices: Vec<Device>,
}

pub struct DeviceRegistry {
    path: Option<PathBuf>,
    devices: Vec<Device>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn random_bytes() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("secure random generation failed"))?;
    Ok(bytes)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

// Both operands have a fixed length. Do not compare digest strings with early exit.
fn digest_matches(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

// Reject symlinks in the target AND its ancestors. Persistence never opens the target for writing.
fn check_path(path: &Path) -> io::Result<()> {
    for component in path.ancestors() {
        if component.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(component) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("credential path contains a symlink"))
            }
            Ok(meta) if component == path && !meta.is_file() => {
                return Err(invalid("credential path is not a regular file"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound && component == path => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    options.mode(0o600);
    // O_NOFOLLOW closes the final-component check/open race on Linux (the supported platform).
    #[cfg(target_os = "linux")]
    options.custom_flags(0o400000);
    options
}

#[cfg(unix)]
fn lock_registry(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let lock_path = path.with_extension("lock");
    check_path(&lock_path)?;
    let file = private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(invalid("credential lock must be a regular mode 0600 file"));
    }
    loop {
        // SAFETY: this is an owned, live fd. flock only changes its advisory lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    // Closing the returned File releases the lock, including all error paths.
}
#[cfg(not(unix))]
fn lock_registry(_: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "credential file locking requires Unix",
    ))
}

impl DeviceRegistry {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let Some(file_path) = path.as_ref() else {
            return Ok(Self {
                path: None,
                devices: Vec::new(),
            });
        };
        check_path(file_path)?;
        let mut options = private_options();
        let file = match options.read(true).open(file_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    devices: Vec::new(),
                })
            }
            Err(e) => return Err(e),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("credentials are not a regular file"));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(invalid("credentials must have mode 0600"));
        }
        let mut body = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut body)?;
        if body.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid("credential file too large"));
        }
        let stored: Stored =
            serde_json::from_slice(&body).map_err(|_| invalid("invalid credential file"))?;
        if stored.v != 1 || stored.devices.len() > MAX_DEVICES {
            return Err(invalid("invalid credential registry version or size"));
        }
        for (i, device) in stored.devices.iter().enumerate() {
            let summary = &device.summary;
            if summary.device_id.is_empty()
                || summary.client_id.is_empty()
                || summary.device_id.len() > MAX_LABEL_BYTES
                || summary.client_id.len() > MAX_LABEL_BYTES
                || summary.name.len() > MAX_LABEL_BYTES
                || stored.devices[..i].iter().any(|other| {
                    other.summary.device_id == summary.device_id
                        || other.summary.client_id == summary.client_id
                        || digest_matches(&other.digest, &device.digest)
                })
            {
                return Err(invalid("invalid or duplicate device record"));
            }
        }
        Ok(Self {
            path,
            devices: stored.devices,
        })
    }

    pub fn issue(&mut self, client_id: &str, name: &str) -> io::Result<(String, String)> {
        self.transaction(|registry| registry.issue_unlocked(client_id, name))
    }

    fn issue_unlocked(&mut self, client_id: &str, name: &str) -> io::Result<(String, String)> {
        if client_id.is_empty() || client_id.len() > MAX_LABEL_BYTES || name.len() > MAX_LABEL_BYTES
        {
            return Err(invalid("invalid device client ID or name"));
        }
        if let Some(grant) = client_id.strip_prefix("grant:") {
            let (owner, client) = grant
                .split_once(':')
                .ok_or_else(|| invalid("invalid grant client ID"))?;
            if client.is_empty()
                || !self
                    .devices
                    .iter()
                    .any(|device| device.summary.device_id == owner)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "grant owner no longer exists",
                ));
            }
        }
        let index = self
            .devices
            .iter()
            .position(|device| device.summary.client_id == client_id);
        if index.is_none() && self.devices.len() >= MAX_DEVICES {
            return Err(io::Error::other("device limit reached"));
        }
        let token = encode_secret(&random_bytes()?)
            .map_err(|_| io::Error::other("token encoding failed"))?;
        let device_id = match index {
            Some(i) => self.devices[i].summary.device_id.clone(),
            None => uuid::Builder::from_random_bytes(random_bytes()?[..16].try_into().unwrap())
                .into_uuid()
                .to_string(),
        };
        let device = Device {
            summary: DeviceSummary {
                device_id: device_id.clone(),
                client_id: client_id.into(),
                name: name.into(),
            },
            digest: digest(&token),
        };
        let mut next = self.devices.clone();
        if let Some(i) = index {
            next[i] = device;
        } else {
            next.push(device);
        }
        self.persist(&next)?;
        self.devices = next;
        Ok((device_id, token))
    }

    pub fn authenticate(&self, token: &str) -> Option<String> {
        let expected = digest(token);
        // Visit every record, even when an earlier one matches.
        let mut matched = None;
        for device in &self.devices {
            if digest_matches(&device.digest, &expected) {
                matched = Some(device.summary.device_id.clone());
            }
        }
        matched
    }

    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        self.transaction(|registry| registry.revoke_unlocked(id))
    }

    fn revoke_unlocked(&mut self, id: &str) -> io::Result<bool> {
        let mut revoked = std::collections::HashSet::from([id.to_owned()]);
        loop {
            let before = revoked.len();
            for device in &self.devices {
                if revoked.iter().any(|owner| {
                    device
                        .summary
                        .client_id
                        .starts_with(&format!("grant:{owner}:"))
                }) {
                    revoked.insert(device.summary.device_id.clone());
                }
            }
            if revoked.len() == before {
                break;
            }
        }
        let mut next = self.devices.clone();
        next.retain(|device| !revoked.contains(&device.summary.device_id));
        if next.len() == self.devices.len() {
            return Ok(false);
        }
        self.persist(&next)?;
        self.devices = next;
        Ok(true)
    }

    pub fn list(&self) -> Vec<DeviceSummary> {
        self.devices
            .iter()
            .map(|device| device.summary.clone())
            .collect()
    }

    // A fresh snapshot must be loaded while holding the lock: stale handles must not
    // resurrect a CLI-revoked credential when the HTTP worker subsequently issues one.
    fn transaction<T>(&mut self, change: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        let Some(path) = &self.path else {
            return change(self);
        };
        let _lock = lock_registry(path)?;
        let mut current = Self::open(Some(path.clone()))?;
        let result = change(&mut current)?;
        self.devices = current.devices;
        Ok(result)
    }

    fn persist(&self, devices: &[Device]) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        check_path(path)?;
        let directory = parent(path);
        let temporary = directory.join(format!(
            ".wayclip-devices-{}.tmp",
            hex::encode(random_bytes()?)
        ));
        let mut created = false;
        let result = (|| {
            let mut options = private_options();
            let mut file = options.write(true).create_new(true).open(&temporary)?;
            created = true;
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer(
                &mut file,
                &Stored {
                    v: 1,
                    devices: devices.to_vec(),
                },
            )
            .map_err(io::Error::other)?;
            file.flush()?;
            file.sync_all()?;
            check_path(path)?;
            fs::rename(&temporary, path)?;
            File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wayclip-credentials-{}",
                hex::encode(random_bytes().unwrap())
            ));
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("devices.json")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn issued_device_ids_are_v4_uuids() {
        let mut registry = DeviceRegistry::open(None).unwrap();
        let (id, _) = registry.issue("phone", "phone").unwrap();
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(parsed.to_string(), id);
    }

    #[test]
    fn stale_handle_cannot_resurrect_revoked_tokens_or_drop_new_devices() {
        let directory = Directory::new();
        let path = directory.file();
        let mut first = DeviceRegistry::open(Some(path.clone())).unwrap();
        let (id, revoked) = first.issue("phone", "phone").unwrap();
        let mut stale = DeviceRegistry::open(Some(path.clone())).unwrap();
        first.revoke(&id).unwrap();
        let (_, fresh) = stale.issue("other", "other").unwrap();
        let current = DeviceRegistry::open(Some(path.clone())).unwrap();
        assert!(current.authenticate(&revoked).is_none());
        assert!(current.authenticate(&fresh).is_some());
        assert_eq!(current.list().len(), 1);
        // First is now stale too: its next issuance must retain the second handle's addition.
        first.issue("third", "third").unwrap();
        let current = DeviceRegistry::open(Some(path)).unwrap();
        assert!(current.authenticate(&fresh).is_some());
        assert_eq!(current.list().len(), 2);
    }

    #[test]
    fn revocation_cascades_through_grants_but_not_other_owners() {
        let mut registry = DeviceRegistry::open(None).unwrap();
        let (root, root_token) = registry.issue("root", "root").unwrap();
        let (child, child_token) = registry
            .issue(&format!("grant:{root}:child"), "child")
            .unwrap();
        let (_, grandchild_token) = registry
            .issue(&format!("grant:{child}:nested"), "grandchild")
            .unwrap();
        let (_, unrelated) = registry.issue("other", "other").unwrap();
        assert!(registry.revoke(&root).unwrap());
        for token in [root_token, child_token, grandchild_token] {
            assert!(registry.authenticate(&token).is_none());
        }
        assert!(registry.authenticate(&unrelated).is_some());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn revoked_owner_cannot_issue_orphan_grant_through_stale_handle() {
        let directory = Directory::new();
        let path = directory.file();
        let mut owner = DeviceRegistry::open(Some(path.clone())).unwrap();
        let (id, token) = owner.issue("phone", "phone").unwrap();
        let mut revoker = DeviceRegistry::open(Some(path.clone())).unwrap();
        revoker.revoke(&id).unwrap();
        // This old authenticated handle still has the parent's record in memory.
        assert!(owner.authenticate(&token).is_some());
        let error = owner
            .issue(&format!("grant:{id}:orphan"), "orphan")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(DeviceRegistry::open(Some(path)).unwrap().list().is_empty());
        let mut memory = DeviceRegistry::open(None).unwrap();
        assert!(memory.issue("grant:missing:orphan", "orphan").is_err());
        assert!(memory.issue("grant:malformed", "orphan").is_err());
        let (id, _) = memory.issue("real", "real").unwrap();
        assert!(memory
            .issue(&format!("grant:{id}:"), "empty client")
            .is_err());
        memory.revoke(&id).unwrap();
        assert!(memory
            .issue(&format!("grant:{id}:orphan"), "orphan")
            .is_err());
    }

    #[test]
    fn simultaneous_handles_serialize_issuance_without_lost_updates() {
        let directory = Directory::new();
        let path = directory.file();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut registry = DeviceRegistry::open(Some(path)).unwrap();
                    barrier.wait();
                    registry.issue(&format!("phone-{i}"), "phone").unwrap()
                })
            })
            .collect();
        let issued: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        let current = DeviceRegistry::open(Some(path)).unwrap();
        assert_eq!(current.list().len(), 2);
        for (id, token) in issued {
            assert_eq!(current.authenticate(&token), Some(id));
        }
    }

    #[test]
    fn restart_authentication_and_no_plaintext_token_on_disk() {
        let directory = Directory::new();
        let path = directory.file();
        let mut registry = DeviceRegistry::open(Some(path.clone())).unwrap();
        let (id, token) = registry.issue("phone", "手机").unwrap();
        assert_eq!(token.len(), 43);
        assert!(!String::from_utf8(fs::read(&path).unwrap())
            .unwrap()
            .contains(&token));
        drop(registry);
        let registry = DeviceRegistry::open(Some(path)).unwrap();
        assert_eq!(registry.authenticate(&token), Some(id));
        assert_eq!(registry.authenticate("bad"), None);
        assert_eq!(registry.list()[0].name, "手机");
    }

    #[test]
    fn rotation_and_revocation_are_isolated_and_persisted() {
        let directory = Directory::new();
        let path = directory.file();
        let mut registry = DeviceRegistry::open(Some(path.clone())).unwrap();
        let (a, old) = registry.issue("a", "A").unwrap();
        let (b, token_b) = registry.issue("b", "B").unwrap();
        let (same_a, new) = registry.issue("a", "A renamed").unwrap();
        assert_eq!(a, same_a);
        assert_ne!(old, new);
        assert_eq!(registry.authenticate(&old), None);
        assert_eq!(registry.authenticate(&new), Some(a.clone()));
        assert_eq!(registry.authenticate(&token_b), Some(b.clone()));
        assert_eq!(registry.list().len(), 2);
        assert!(registry.revoke(&a).unwrap());
        assert!(!registry.revoke(&a).unwrap());
        let registry = DeviceRegistry::open(Some(path)).unwrap();
        assert_eq!(registry.authenticate(&new), None);
        assert_eq!(registry.authenticate(&token_b), Some(b));
    }

    #[test]
    fn memory_mode_limit_and_randomness() {
        let mut registry = DeviceRegistry::open(None).unwrap();
        let mut tokens = std::collections::HashSet::new();
        for i in 0..MAX_DEVICES {
            let (_, token) = registry.issue(&i.to_string(), "phone").unwrap();
            assert!(tokens.insert(token));
        }
        assert!(registry.issue("overflow", "phone").is_err());
        assert_eq!(registry.list().len(), MAX_DEVICES);
        assert!(registry.issue("0", "rotated at limit").is_ok());
    }

    #[test]
    fn corrupt_registry_fails_closed() {
        let directory = Directory::new();
        let path = directory.file();
        let mut registry = DeviceRegistry::open(Some(path.clone())).unwrap();
        registry.issue("phone", "phone").unwrap();
        for body in [b"{".as_slice(), b"null", b"{\"v\":2,\"devices\":[]}",
            b"{\"v\":1,\"devices\":[{\"summary\":{\"device_id\":\"x\",\"client_id\":\"x\",\"name\":\"x\"},\"digest\":[0]}]}"] {
            fs::write(&path, body).unwrap();
            assert!(DeviceRegistry::open(Some(path.clone())).is_err());
        }
    }

    #[test]
    fn write_failure_does_not_mutate_memory() {
        let directory = Directory::new();
        let path = directory.file();
        let mut registry = DeviceRegistry::open(Some(path.clone())).unwrap();
        let (id, token) = registry.issue("phone", "original").unwrap();
        let before = registry.list();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(registry.issue("phone", "changed").is_err());
        assert!(registry.issue("other", "new").is_err());
        assert!(registry.revoke(&id).is_err());
        assert_eq!(registry.list(), before);
        assert_eq!(registry.authenticate(&token), Some(id));
    }

    #[cfg(unix)]
    #[test]
    fn enforces_private_permissions_and_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let path = directory.file();
        let mut registry = DeviceRegistry::open(Some(path.clone())).unwrap();
        registry.issue("phone", "phone").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(DeviceRegistry::open(Some(path.clone())).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = directory.0.join("alias");
        symlink(&path, &alias).unwrap();
        assert!(DeviceRegistry::open(Some(alias)).is_err());
        let parent_alias = directory.0.join("parent-alias");
        symlink(&directory.0, &parent_alias).unwrap();
        assert!(DeviceRegistry::open(Some(parent_alias.join("devices.json"))).is_err());
        // Replacing the target with a symlink must fail without changing the symlink's target.
        let original = fs::read(&path).unwrap();
        let target = directory.0.join("real.json");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(registry.issue("phone", "changed").is_err());
        assert_eq!(fs::read(target).unwrap(), original);
    }
}
