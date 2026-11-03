use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatus {
    pub v: u8,
    pub sync_paused: bool,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub push: bool,
}
impl SyncStatus {
    pub fn conservative(send: bool, receive: bool) -> Self {
        Self {
            v: 2,
            sync_paused: false,
            send_enabled: send,
            receive_enabled: receive,
            push: false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn push_defaults_false() {
        assert!(!SyncStatus::conservative(true, true).push);
    }
}
