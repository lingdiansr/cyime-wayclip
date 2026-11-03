use super::{ClipboardProfile, Stamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct V2Clipboard {
    pub v: u8,
    pub profile: ClipboardProfile,
    pub stamp: Stamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatus {
    pub v: u8,
    pub sync_paused: bool,
    pub send_enabled: bool,
    pub receive_enabled: bool,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            v: 2,
            sync_paused: false,
            send_enabled: true,
            receive_enabled: true,
        }
    }
}

impl V2Clipboard {
    pub fn new(profile: ClipboardProfile, stamp: Stamp) -> Self {
        Self {
            v: 2,
            profile,
            stamp,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.v != 2 {
            return Err("unsupported protocol version");
        }
        self.profile
            .validate()
            .map_err(|_| "invalid clipboard profile")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v2_round_trip() {
        let x = V2Clipboard::new(
            ClipboardProfile::text("hello").unwrap(),
            Stamp::new("linux", 1),
        );
        let b = serde_json::to_vec(&x).unwrap();
        let y: V2Clipboard = serde_json::from_slice(&b).unwrap();
        assert_eq!(x, y);
    }
    #[test]
    fn rejects_wrong_version() {
        let x = V2Clipboard {
            v: 1,
            profile: ClipboardProfile::text("x").unwrap(),
            stamp: Stamp::new("a", 1),
        };
        assert!(x.validate().is_err());
    }
    #[test]
    fn status_defaults_to_v2_and_enabled() {
        let s = SyncStatus::default();
        assert_eq!(
            (s.v, s.sync_paused, s.send_enabled, s.receive_enabled),
            (2, false, true, true)
        );
    }
}
