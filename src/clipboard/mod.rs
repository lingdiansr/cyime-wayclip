use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Starting,
    Ready,
    Unsupported,
    Disconnected,
    Reconnecting,
}

/// An immutable selection from one compositor connection. Generation is used
/// for conditional writes; session prevents a reconnect being mistaken for a copy.
#[derive(Debug, Clone)]
pub struct ClipboardSnapshot {
    pub generation: u64,
    pub session: u64,
    pub text: Option<Arc<str>>,
}

#[cfg(feature = "wayland")]
pub mod wayland;
