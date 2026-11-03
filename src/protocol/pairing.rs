use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairingError {
    #[error("secret must be 32 bytes")]
    SecretLength,
    #[error("invalid base64url secret")]
    Encoding,
    #[error("secret has invalid length")]
    InvalidEncodedLength,
}

pub fn encode_secret(secret: &[u8]) -> Result<String, PairingError> {
    if secret.len() != 32 {
        return Err(PairingError::SecretLength);
    }
    Ok(URL_SAFE_NO_PAD.encode(secret))
}
pub fn decode_secret(encoded: &str) -> Result<[u8; 32], PairingError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| PairingError::Encoding)?;
    bytes
        .try_into()
        .map_err(|_| PairingError::InvalidEncodedLength)
}
pub fn token_digest(token: &[u8]) -> String {
    Sha256::digest(token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_32_bytes() {
        let b = [7u8; 32];
        let s = encode_secret(&b).unwrap();
        assert_eq!(s.len(), 43);
        assert_eq!(decode_secret(&s).unwrap(), b);
    }
    #[test]
    fn reject_wrong_size() {
        assert_eq!(encode_secret(&[0; 31]), Err(PairingError::SecretLength));
        assert!(decode_secret("abc").is_err());
    }
    #[test]
    fn digest_is_hex() {
        assert_eq!(token_digest(b"x").len(), 64);
    }
}

pub fn generate_pairing_code() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    token_digest(nanos.to_string().as_bytes())[..32].to_string()
}
