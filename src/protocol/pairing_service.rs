use super::{encode_secret, PairOffer};
use sha2::Digest;
use thiserror::Error;
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairError {
    #[error("offer unavailable")]
    Unavailable,
    #[error("offer expired")]
    Expired,
    #[error("offer already consumed")]
    Consumed,
    #[error("invalid secret")]
    Secret,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PairResponse {
    pub v: u8,
    pub server_id: String,
    pub server_name: String,
    pub device_id: String,
    pub access_token: String,
}
pub struct PairingService {
    offer: Option<PairOffer>,
    consumed: bool,
    server_id: String,
    server_name: String,
}
impl PairingService {
    pub fn new(server_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            offer: None,
            consumed: false,
            server_id: server_id.into(),
            server_name: name.into(),
        }
    }
    pub fn expected_secret_hash(&self) -> String {
        self.offer
            .as_ref()
            .map(|o| format!("{}", hex::encode(sha2::Sha256::digest(o.secret.as_bytes()))))
            .unwrap_or_default()
    }
    pub fn set_offer(&mut self, offer: PairOffer) {
        self.offer = Some(offer);
        self.consumed = false;
    }
    pub fn pair(
        &mut self,
        secret: &str,
        device_id: impl Into<String>,
        now: u64,
    ) -> Result<PairResponse, PairError> {
        let o = self.offer.as_ref().ok_or(PairError::Unavailable)?;
        if self.consumed {
            return Err(PairError::Consumed);
        }
        if o.expires_at <= now {
            return Err(PairError::Expired);
        }
        if o.secret != secret {
            return Err(PairError::Secret);
        }
        self.consumed = true;
        let token = encode_secret(&[7; 32]).map_err(|_| PairError::Secret)?;
        Ok(PairResponse {
            v: 2,
            server_id: self.server_id.clone(),
            server_name: self.server_name.clone(),
            device_id: device_id.into(),
            access_token: token,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn offer() -> PairOffer {
        PairOffer {
            v: 2,
            server_id: "s".into(),
            name: "n".into(),
            certificate_sha256: "a".repeat(64),
            pairing_id: "p".into(),
            secret: encode_secret(&[1; 32]).unwrap(),
            endpoints: vec!["https://127.0.0.1:1".into()],
            expires_at: 10,
        }
    }
    #[test]
    fn one_shot() {
        let mut s = PairingService::new("s", "n");
        s.set_offer(offer());
        assert!(s.pair(&offer().secret, "d", 1).is_ok());
        assert_eq!(s.pair(&offer().secret, "d", 1), Err(PairError::Consumed));
    }
}
