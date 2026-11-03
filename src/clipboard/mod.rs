use crate::protocol::{ClipboardProfile, ProfileError};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Starting,
    Ready,
    Unsupported,
    Disconnected,
    Reconnecting,
}
pub trait ClipboardProvider {
    fn status(&self) -> ProviderStatus;
    fn read(&mut self) -> Result<Option<ClipboardProfile>, ProfileError>;
    fn write(&mut self, profile: &ClipboardProfile) -> Result<(), ProfileError>;
}
#[derive(Debug, Default)]
pub struct UnavailableProvider;
impl ClipboardProvider for UnavailableProvider {
    fn status(&self) -> ProviderStatus {
        ProviderStatus::Unsupported
    }
    fn read(&mut self) -> Result<Option<ClipboardProfile>, ProfileError> {
        Ok(None)
    }
    fn write(&mut self, _: &ClipboardProfile) -> Result<(), ProfileError> {
        Err(ProfileError::InvalidUtf8)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_is_explicit() {
        let mut p = UnavailableProvider;
        assert_eq!(p.status(), ProviderStatus::Unsupported);
        assert!(p.read().unwrap().is_none());
    }
}
pub mod wayland;
