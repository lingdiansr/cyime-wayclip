use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
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
    let fingerprint = Sha256::digest(&cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
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
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("server-id");
    if let Ok(v) = fs::read_to_string(&path) {
        if let Ok(id) = uuid::Uuid::parse_str(v.trim()) {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    write_private(&path, id.as_bytes()).map_err(|e| e.to_string())?;
    Ok(id)
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
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let cert_path = dir.join("tls-cert.pem");
    let key_path = dir.join("tls-key.pem");
    if cert_path.exists() && key_path.exists() {
        let cert_pem = fs::read(&cert_path).map_err(|e| e.to_string())?;
        let key_pem = fs::read(&key_path).map_err(|e| e.to_string())?;
        let cert_der = rustls_pemfile::certs(&mut &cert_pem[..])
            .next()
            .ok_or("invalid certificate")?
            .map_err(|e| e.to_string())?
            .to_vec();
        let fingerprint = hex::encode(Sha256::digest(&cert_der));
        return Ok(Identity {
            cert_der,
            key_der: Vec::new(),
            cert_pem,
            key_pem,
            fingerprint,
        });
    }
    let id = generate_identity(name)?;
    write_private(&key_path, &id.key_pem).map_err(|e| e.to_string())?;
    fs::write(&cert_path, &id.cert_pem).map_err(|e| e.to_string())?;
    Ok(id)
}
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn load_or_generate_identity(name: &str, dir: &Path) -> Result<Identity, String> {
    let cert_path = dir.join("server-cert.pem");
    let key_path = dir.join("server-key.pem");
    if cert_path.exists() && key_path.exists() {
        let cert_pem = fs::read(&cert_path).map_err(|e| e.to_string())?;
        let key_pem = fs::read(&key_path).map_err(|e| e.to_string())?;
        let cert_der = rustls_pemfile::certs(&mut &cert_pem[..])
            .next()
            .ok_or("invalid certificate")?
            .map_err(|e| e.to_string())?
            .to_vec();
        let fingerprint = Sha256::digest(&cert_der)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        return Ok(Identity {
            cert_der,
            key_der: Vec::new(),
            cert_pem,
            key_pem,
            fingerprint,
        });
    }
    let identity = generate_identity(name)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    fs::write(&cert_path, &identity.cert_pem).map_err(|e| e.to_string())?;
    fs::write(&key_path, &identity.key_pem).map_err(|e| e.to_string())?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
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
