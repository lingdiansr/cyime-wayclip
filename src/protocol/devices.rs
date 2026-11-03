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
        if client_id.is_empty() || client_id.len() > MAX_LABEL_BYTES || name.len() > MAX_LABEL_BYTES
        {
            return Err(invalid("invalid device client ID or name"));
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
            None => hex::encode(random_bytes()?),
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
        let mut next = self.devices.clone();
        next.retain(|device| device.summary.device_id != id);
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
