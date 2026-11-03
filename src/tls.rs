use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub fingerprint: String,
}
pub fn generate_identity(name: &str) -> Result<Identity, String> {
    let c = generate_simple_self_signed(vec![name.into()]).map_err(|e| e.to_string())?;
    let cert = c.cert.der().to_vec();
    let cert_pem = c.cert.pem().into_bytes();
    let key = c.key_pair.serialize_der();
    let key_pem = c.key_pair.serialize_pem().into_bytes();
    let fingerprint = hex::encode(Sha256::digest(&cert));
    Ok(Identity {
        cert_der: cert,
        key_der: key,
        cert_pem,
        key_pem,
        fingerprint,
    })
}
pub fn stable_server_id() -> Result<String, String> {
    let Some(dir) = identity_path() else {
        return Ok(uuid::Uuid::new_v4().to_string());
    };
    stable_server_id_at(&dir)
}

fn stable_server_id_at(dir: &Path) -> Result<String, String> {
    private_directory(dir).map_err(|e| e.to_string())?;
    let _lock = identity_lock(dir).map_err(|e| e.to_string())?;
    let path = dir.join("server-id");
    match read_file(&path, true) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
            let id = uuid::Uuid::parse_str(text.trim())
                .map_err(|e| format!("invalid existing server ID: {e}"))?;
            Ok(id.to_string())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let id = uuid::Uuid::new_v4().to_string();
            write_private(&path, id.as_bytes()).map_err(|e| e.to_string())?;
            Ok(id)
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn identity_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("cyime-wayclip"))
}

pub fn load_or_create_identity(name: &str) -> Result<Identity, String> {
    let Some(dir) = identity_path() else {
        return generate_identity(name);
    };
    load_or_create_identity_at(&dir, name)
}

pub fn load_or_create_identity_at(dir: &Path, name: &str) -> Result<Identity, String> {
    load_identity_at(dir, name, "tls-cert.pem", "tls-key.pem")
}

pub fn load_or_generate_identity(name: &str, dir: &Path) -> Result<Identity, String> {
    load_identity_at(dir, name, "server-cert.pem", "server-key.pem")
}

fn load_identity_at(
    dir: &Path,
    name: &str,
    cert_name: &str,
    key_name: &str,
) -> Result<Identity, String> {
    private_directory(dir).map_err(|e| e.to_string())?;
    let _lock = identity_lock(dir).map_err(|e| e.to_string())?;
    let cert_path = dir.join(cert_name);
    let key_path = dir.join(key_name);
    let cert = read_file(&cert_path, false);
    let key = read_file(&key_path, true);
    match (cert, key) {
        (Ok(cert_pem), Ok(key_pem)) => parse_identity(cert_pem, key_pem),
        (Err(cert), Err(key))
            if cert.kind() == io::ErrorKind::NotFound && key.kind() == io::ErrorKind::NotFound =>
        {
            let identity = generate_identity(name)?;
            // A crash between these writes leaves a partial identity, which fails closed.
            // Never regenerate a partially persisted pinned identity on the next startup.
            write_private(&key_path, &identity.key_pem).map_err(|e| e.to_string())?;
            write_private(&cert_path, &identity.cert_pem).map_err(|e| e.to_string())?;
            Ok(identity)
        }
        (Err(error), _) if error.kind() != io::ErrorKind::NotFound => Err(error.to_string()),
        (_, Err(error)) if error.kind() != io::ErrorKind::NotFound => Err(error.to_string()),
        _ => Err("incomplete TLS identity: certificate and private key must both exist".into()),
    }
}

fn parse_identity(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Result<Identity, String> {
    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let cert_der = certs.first().ok_or("invalid certificate")?.to_vec();
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| e.to_string())?
        .ok_or("invalid private key")?;
    let key_der = key.secret_der().to_vec();
    rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| e.to_string())?;
    let fingerprint = hex::encode(Sha256::digest(&cert_der));
    Ok(Identity {
        cert_der,
        key_der,
        cert_pem,
        key_pem,
        fingerprint,
    })
}

// Do not follow symlinks, including dangling ones and any existing ancestor.
fn reject_symlinks(path: &Path) -> io::Result<()> {
    for component in path.ancestors() {
        if component.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "TLS identity path must not contain symlinks",
                ));
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    options
}

fn private_directory(dir: &Path) -> io::Result<()> {
    reject_symlinks(dir)?;
    // Create one component at a time with private permissions, never a public
    // directory which is tightened only after credentials have been written.
    if !dir.as_os_str().is_empty() && !dir.exists() {
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            private_directory_create(parent)?;
        }
        private_directory_create(dir)?;
    }
    reject_symlinks(dir)?;
    let mut opts = options();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY);
    }
    let directory = opts.open(dir)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::other("identity directory is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_directory_create(dir: &Path) -> io::Result<()> {
    reject_symlinks(dir)?;
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_directory_create(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => reject_symlinks(dir),
        result => result,
    }
}

fn read_file(path: &Path, private: bool) -> io::Result<Vec<u8>> {
    reject_symlinks(path)?;
    let file = options().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(io::Error::other("invalid identity file type or size"));
    }
    if private {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // fchmod the opened, NOFOLLOW handle, not a path which could be swapped.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(io::Error::other("identity file too large"));
    }
    Ok(bytes)
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    reject_symlinks(path)?;
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    private_directory(directory)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let temporary = directory.join(format!(".identity-{}.tmp", hex::encode(random)));
    let mut created = false;
    let result = (|| {
        let mut file = options().write(true).create_new(true).open(&temporary)?;
        created = true;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Restrictive umasks must not leave a persisted unreadable identity.
            // Permissions are set on the open handle before any secret bytes.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        reject_symlinks(path)?;
        fs::rename(&temporary, path)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if created && result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn identity_lock(dir: &Path) -> io::Result<File> {
    let path = dir.join(".identity.lock");
    reject_symlinks(&path)?;
    let file = options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("invalid identity lock file"));
    }
    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        loop {
            // SAFETY: the descriptor is owned and remains live until lock drop.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("cyime-tls-test-{}", uuid::Uuid::new_v4())))
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn actual_server_identity_survives_restart_and_has_private_modes() {
        let directory = TestDirectory::new();
        let a = load_or_generate_identity("localhost", &directory.0).unwrap();
        let b = load_or_generate_identity("localhost", &directory.0).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.key_der, b.key_der);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(directory.0.join("server-key.pem"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(fs::read_dir(&directory.0).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_public_key_and_directory_permissions_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TestDirectory::new();
        let identity = load_or_generate_identity("localhost", &directory.0).unwrap();
        let key = directory.0.join("server-key.pem");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
        let reloaded = load_or_generate_identity("localhost", &directory.0).unwrap();
        assert_eq!(identity.fingerprint, reloaded.fingerprint);
        assert_eq!(
            fs::metadata(key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn partial_identity_never_replaces_surviving_half() {
        for missing in ["server-cert.pem", "server-key.pem"] {
            let directory = TestDirectory::new();
            load_or_generate_identity("localhost", &directory.0).unwrap();
            let survivor = if missing == "server-cert.pem" {
                "server-key.pem"
            } else {
                "server-cert.pem"
            };
            let before = fs::read(directory.0.join(survivor)).unwrap();
            fs::remove_file(directory.0.join(missing)).unwrap();
            assert!(load_or_generate_identity("localhost", &directory.0).is_err());
            assert_eq!(fs::read(directory.0.join(survivor)).unwrap(), before);
            assert!(!directory.0.join(missing).exists());
        }
        let directory = TestDirectory::new();
        load_or_create_identity_at(&directory.0, "localhost").unwrap();
        fs::remove_file(directory.0.join("tls-key.pem")).unwrap();
        assert!(load_or_create_identity_at(&directory.0, "localhost").is_err());
    }

    #[test]
    fn corrupt_and_mismatched_keys_and_certificates_never_regenerate() {
        for corrupt in ["server-key.pem", "server-cert.pem"] {
            let directory = TestDirectory::new();
            load_or_generate_identity("localhost", &directory.0).unwrap();
            fs::write(directory.0.join(corrupt), b"invalid identity").unwrap();
            let key = fs::read(directory.0.join("server-key.pem")).unwrap();
            let cert = fs::read(directory.0.join("server-cert.pem")).unwrap();
            assert!(load_or_generate_identity("localhost", &directory.0).is_err());
            assert_eq!(fs::read(directory.0.join("server-key.pem")).unwrap(), key);
            assert_eq!(fs::read(directory.0.join("server-cert.pem")).unwrap(), cert);
        }
        let directory = TestDirectory::new();
        let original = load_or_generate_identity("localhost", &directory.0).unwrap();
        let other = generate_identity("localhost").unwrap();
        write_private(&directory.0.join("server-key.pem"), &other.key_pem).unwrap();
        assert!(load_or_generate_identity("localhost", &directory.0).is_err());
        assert_eq!(
            fs::read(directory.0.join("server-cert.pem")).unwrap(),
            original.cert_pem
        );
        assert_eq!(
            fs::read(directory.0.join("server-key.pem")).unwrap(),
            other.key_pem
        );
    }

    #[test]
    fn invalid_existing_server_id_is_preserved_and_rejected() {
        let directory = TestDirectory::new();
        let id = stable_server_id_at(&directory.0).unwrap();
        assert_eq!(stable_server_id_at(&directory.0).unwrap(), id);
        let path = directory.0.join("server-id");
        fs::write(&path, b"invalid-uuid").unwrap();
        assert!(stable_server_id_at(&directory.0).is_err());
        assert_eq!(fs::read(path).unwrap(), b"invalid-uuid");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_keys_certificates_directories_and_ancestors_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = TestDirectory::new();
        let real = directory.0.join("real");
        load_or_generate_identity("localhost", &real).unwrap();
        let alias = directory.0.join("alias");
        symlink(&real, &alias).unwrap();
        assert!(load_or_generate_identity("localhost", &alias).is_err());
        assert!(load_or_generate_identity("localhost", &alias.join("nested")).is_err());
        for name in [
            "server-key.pem",
            "server-cert.pem",
            "server-id",
            ".identity.lock",
        ] {
            let identity_dir = directory.0.join(name.replace('.', "-"));
            private_directory(&identity_dir).unwrap();
            symlink(directory.0.join("nonexistent"), identity_dir.join(name)).unwrap();
            if name == "server-id" {
                assert!(stable_server_id_at(&identity_dir).is_err());
            } else {
                assert!(load_or_generate_identity("localhost", &identity_dir).is_err());
            }
            assert!(!directory.0.join("nonexistent").exists());
        }
    }

    #[test]
    fn fingerprint_is_leaf_sha256() {
        let i = generate_identity("localhost").unwrap();
        assert_eq!(i.fingerprint.len(), 64);
    }
    #[test]
    fn identity_survives_restart() {
        let d = std::env::temp_dir().join(format!("cyime-tls-{}", std::process::id()));
        let a = load_or_create_identity_at(&d, "localhost").unwrap();
        let b = load_or_create_identity_at(&d, "localhost").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(d.join("tls-key.pem"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(d);
    }
}
