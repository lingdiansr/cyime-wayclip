use crate::{
    clipboard::ProviderStatus,
    config::Config,
    protocol::{get, put, ClipboardProfile, GetResult, PutResult},
};
#[derive(Debug)]
pub struct Server<P> {
    pub config: Config,
    pub provider_status: ProviderStatus,
    pub profile: Option<ClipboardProfile>,
    _provider: std::marker::PhantomData<P>,
}
impl<P> Server<P> {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            provider_status: ProviderStatus::Starting,
            profile: None,
            _provider: std::marker::PhantomData,
        }
    }
    pub fn get(&self, etag: Option<&str>) -> GetResult {
        get(
            self.profile.as_ref(),
            etag,
            self.config.send_enabled,
            self.provider_status == ProviderStatus::Ready,
        )
    }
    pub fn put(&mut self, body: &[u8], authenticated: bool) -> PutResult {
        let r = put(
            body,
            self.config.receive_enabled,
            authenticated,
            self.provider_status == ProviderStatus::Ready,
        );
        if let PutResult::Accepted(ref p) = r {
            self.profile = Some(p.clone());
        }
        r
    }
    pub fn mark_ready(&mut self) {
        self.provider_status = ProviderStatus::Ready;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_returns_service_state() {
        let s: Server<()> = Server::new(Config::default());
        assert_eq!(s.get(None), GetResult::Unavailable);
    }
}
