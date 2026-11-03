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
    #[error("secure random generation failed")]
    Entropy,
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
            .map(|o| hex::encode(sha2::Sha256::digest(o.secret.as_bytes())))
            .unwrap_or_default()
    }
    pub fn set_offer(&mut self, offer: PairOffer) {
        self.offer = Some(offer);
        self.consumed = false;
    }
    /// Validate before issuing durable credentials; does not spend the offer.
    pub fn check_offer(&self, secret: &str, now: u64) -> Result<(), PairError> {
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
        Ok(())
    }
    /// Call only after check_offer and durable credential issuance both succeed.
    pub fn consume_offer(&mut self) {
        self.consumed = true;
    }
    pub fn pair(
        &mut self,
        secret: &str,
        device_id: impl Into<String>,
        now: u64,
    ) -> Result<PairResponse, PairError> {
        self.pair_with_entropy(secret, device_id.into(), now, |bytes| {
            getrandom::fill(bytes).map_err(|_| PairError::Entropy)
        })
    }
    fn pair_with_entropy(
        &mut self,
        secret: &str,
        device_id: String,
        now: u64,
        entropy: impl FnOnce(&mut [u8; 32]) -> Result<(), PairError>,
    ) -> Result<PairResponse, PairError> {
        self.check_offer(secret, now)?;
        let mut bytes = [0u8; 32];
        entropy(&mut bytes)?;
        let token = encode_secret(&bytes).map_err(|_| PairError::Entropy)?;
        self.consume_offer();
        Ok(PairResponse {
            v: 2,
            server_id: self.server_id.clone(),
            server_name: self.server_name.clone(),
            device_id,
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
    fn independent_offers_issue_unpredictable_tokens() {
        let mut tokens = std::collections::HashSet::new();
        for _ in 0..32 {
            let mut s = PairingService::new("s", "n");
            s.set_offer(offer());
            let token = s.pair(&offer().secret, "d", 1).unwrap().access_token;
            assert_eq!(super::super::decode_secret(&token).unwrap().len(), 32);
            assert_ne!(token, encode_secret(&[7; 32]).unwrap());
            assert!(tokens.insert(token));
        }
    }
    #[test]
    fn rejected_attempt_does_not_consume_offer() {
        let mut s = PairingService::new("s", "n");
        s.set_offer(offer());
        assert_eq!(s.pair("wrong", "d", 1), Err(PairError::Secret));
        assert!(s.pair(&offer().secret, "d", 1).is_ok());
    }
    #[test]
    fn entropy_failure_does_not_consume_offer() {
        let mut s = PairingService::new("s", "n");
        s.set_offer(offer());
        assert_eq!(
            s.pair_with_entropy(&offer().secret, "d".into(), 1, |_| Err(PairError::Entropy)),
            Err(PairError::Entropy)
        );
        assert!(s.pair(&offer().secret, "d", 1).is_ok());
    }
    #[test]
    fn check_and_explicit_consume_support_durable_issuance() {
        let mut s = PairingService::new("s", "n");
        s.set_offer(offer());
        assert_eq!(s.check_offer(&offer().secret, 1), Ok(()));
        assert_eq!(s.check_offer(&offer().secret, 1), Ok(()));
        assert_eq!(s.check_offer("wrong", 1), Err(PairError::Secret));
        assert_eq!(s.check_offer(&offer().secret, 10), Err(PairError::Expired));
        s.consume_offer();
        assert_eq!(s.check_offer(&offer().secret, 1), Err(PairError::Consumed));
    }
    #[test]
    fn one_shot() {
        let mut s = PairingService::new("s", "n");
        s.set_offer(offer());
        assert!(s.pair(&offer().secret, "d", 1).is_ok());
        assert_eq!(s.pair(&offer().secret, "d", 1), Err(PairError::Consumed));
    }
}
