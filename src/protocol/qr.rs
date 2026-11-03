use super::{decode_secret, PairingError};
use base64::Engine;
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

#[allow(dead_code)]
pub fn encode_compact_offer(offer: &PairOffer) -> Result<String, OfferError> {
    let ep = offer.endpoints.first().ok_or(OfferError::Endpoints)?;
    let rest = ep.strip_prefix("https://").ok_or(OfferError::Endpoint)?;
    let (host, port) = rest.rsplit_once(':').ok_or(OfferError::Endpoint)?;
    let ip: std::net::Ipv4Addr = host.parse().map_err(|_| OfferError::Endpoint)?;
    let port: u16 = port.parse().map_err(|_| OfferError::Endpoint)?;
    let cert = hex::decode(&offer.certificate_sha256).map_err(|_| OfferError::Pin)?;
    if cert.len() != 32 {
        return Err(OfferError::Pin);
    }
    let secret = decode_secret(&offer.secret)?;
    let mut b = vec![2, 1];
    b.extend(ip.octets());
    b.extend(port.to_be_bytes());
    b.extend(
        uuid::Uuid::parse_str(&offer.server_id)
            .map_err(|_| OfferError::Json)?
            .as_bytes(),
    );
    b.extend(cert);
    b.extend(
        uuid::Uuid::parse_str(&offer.pairing_id)
            .map_err(|_| OfferError::Json)?
            .as_bytes(),
    );
    b.extend(secret);
    b.extend(offer.expires_at.to_be_bytes());
    Ok(format!(
        "cyime://pair?v=2&c={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    ))
}

pub fn encode_pair_uri(offer: &PairOffer) -> Result<String, OfferError> {
    let json = serde_json::to_vec(offer).map_err(|_| OfferError::Json)?;
    if let Ok(c) = encode_compact_offer(offer) {
        return Ok(format!(
            "cyime://pair?v=2&c={c}",
            c = c.trim_start_matches("cyime://pair?v=2&c=")
        ));
    }
    Ok(format!(
        "cyime://pair?v=2&data={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    ))
}
#[cfg(test)]
mod uri_tests {
    use super::*;
    #[test]
    fn uri_prefix() {
        let o = PairOffer {
            v: 2,
            server_id: "s".into(),
            name: "n".into(),
            certificate_sha256: "a".repeat(64),
            pairing_id: "p".into(),
            secret: crate::protocol::encode_secret(&[1; 32]).unwrap(),
            endpoints: vec!["https://127.0.0.1:1".into()],
            expires_at: 20,
        };
        assert!(encode_pair_uri(&o)
            .unwrap()
            .starts_with("cyime://pair?v=2&"));
    }
}

#[cfg(test)]
mod android_contract_tests {
    use super::*;

    #[test]
    fn compact_identity_matches_android_pair_response_validation() {
        let server_id = uuid::Uuid::new_v4();
        let pairing_id = uuid::Uuid::new_v4();
        let device_id = uuid::Uuid::new_v4();
        let offer = PairOffer {
            v: 2,
            server_id: server_id.to_string(),
            name: "test".into(),
            certificate_sha256: "ab".repeat(32),
            pairing_id: pairing_id.to_string(),
            secret: super::super::encode_secret(&[9; 32]).unwrap(),
            endpoints: vec!["https://10.42.0.1:9876".into()],
            expires_at: 300,
        };
        let uri = encode_pair_uri(&offer).unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(uri.strip_prefix("cyime://pair?v=2&c=").unwrap())
            .unwrap();
        assert_eq!(bytes.len(), 112);
        assert_eq!(&bytes[8..24], server_id.as_bytes());
        assert_eq!(&bytes[56..72], pairing_id.as_bytes());
        assert_eq!(&bytes[72..104], &[9; 32]);
        let mut service = super::super::PairingService::new(server_id.to_string(), "test");
        service.set_offer(offer.clone());
        let response = service
            .pair(&offer.secret, device_id.to_string(), 1)
            .unwrap();
        // WindowsPairingProtocol.received requires matching server ID and UUID device ID.
        assert_eq!(
            response.server_id,
            uuid::Uuid::from_slice(&bytes[8..24]).unwrap().to_string()
        );
        assert!(uuid::Uuid::parse_str(&response.device_id).is_ok());
        assert_eq!(response.access_token.len(), 43);
    }
}
