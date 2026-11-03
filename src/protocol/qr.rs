use super::{decode_secret, PairingError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairOffer {
    pub v: u8,
    pub server_id: String,
    pub name: String,
    pub certificate_sha256: String,
    pub pairing_id: String,
    pub secret: String,
    pub endpoints: Vec<String>,
    pub expires_at: u64,
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OfferError {
    #[error("invalid version")]
    Version,
    #[error("offer too large")]
    TooLarge,
    #[error("invalid secret: {0}")]
    Secret(#[from] PairingError),
    #[error("invalid endpoint count")]
    Endpoints,
    #[error("invalid endpoint")]
    Endpoint,
    #[error("invalid certificate pin")]
    Pin,
    #[error("invalid expiry")]
    Expiry,
    #[error("invalid JSON")]
    Json,
}
impl PairOffer {
    pub fn validate(&self, now: u64) -> Result<(), OfferError> {
        if self.v != 2 {
            return Err(OfferError::Version);
        }
        if self.endpoints.is_empty() || self.endpoints.len() > 8 {
            return Err(OfferError::Endpoints);
        }
        if self
            .endpoints
            .iter()
            .any(|e| !(e.starts_with("https://") && e.len() > 8))
        {
            return Err(OfferError::Endpoint);
        }
        if self.certificate_sha256.len() != 64
            || !self
                .certificate_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(OfferError::Pin);
        }
        if self.expires_at <= now {
            return Err(OfferError::Expiry);
        }
        decode_secret(&self.secret)?;
        Ok(())
    }
    pub fn from_json(bytes: &[u8], now: u64) -> Result<Self, OfferError> {
        if bytes.len() > 4096 {
            return Err(OfferError::TooLarge);
        }
        let x: Self = serde_json::from_slice(bytes).map_err(|_| OfferError::Json)?;
        x.validate(now)?;
        Ok(x)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn o() -> PairOffer {
        PairOffer {
            v: 2,
            server_id: "s".into(),
            name: "n".into(),
            certificate_sha256: "a".repeat(64),
            pairing_id: "p".into(),
            secret: super::super::encode_secret(&[1; 32]).unwrap(),
            endpoints: vec!["https://127.0.0.1:1".into()],
            expires_at: 20,
        }
    }
    #[test]
    fn valid() {
        assert!(o().validate(1).is_ok());
    }
    #[test]
    fn reject_expired() {
        assert_eq!(o().validate(20), Err(OfferError::Expiry));
    }
}
