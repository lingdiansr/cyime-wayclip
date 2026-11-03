use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_is_leaf_sha256() {
        let i = generate_identity("localhost").unwrap();
        assert_eq!(i.fingerprint.len(), 64);
    }
}
