#[cfg(feature = "wayland")]
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandCapabilities {
    pub connected: bool,
    pub globals: Vec<String>,
}
#[cfg(feature = "wayland")]
pub fn detect() -> WaylandCapabilities {
    let Ok(conn) = Connection::connect_to_env() else {
        return WaylandCapabilities {
            connected: false,
            globals: vec![],
        };
    };
    let Ok((globals, _)) = registry_queue_init::<WaylandCapabilities>(&conn) else {
        return WaylandCapabilities {
            connected: false,
            globals: vec![],
        };
    };
    WaylandCapabilities {
        connected: true,
        globals: globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|g| g.interface)
            .collect(),
    }
}
#[cfg(not(feature = "wayland"))]
pub fn detect() -> WaylandCapabilities {
    WaylandCapabilities {
        connected: false,
        globals: vec![],
    }
}
impl WaylandCapabilities {
    pub fn provider_status(&self) -> super::ProviderStatus {
        if self.supports_data_control() {
            super::ProviderStatus::Ready
        } else if self.connected {
            super::ProviderStatus::Unsupported
        } else {
            super::ProviderStatus::Disconnected
        }
    }
    pub fn supports_data_control(&self) -> bool {
        self.globals.iter().any(|g| g.contains("data_control"))
    }
    pub fn diagnostic(&self) -> &'static str {
        if !self.connected {
            "Wayland unavailable: cannot connect to compositor"
        } else if !self.supports_data_control() {
            "Wayland connected but no data-control protocol advertised"
        } else {
            "Wayland data-control protocol detected"
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_is_explicit() {
        let c = WaylandCapabilities {
            connected: false,
            globals: vec![],
        };
        assert!(!c.supports_data_control());
        assert!(c.diagnostic().contains("unavailable"));
    }
    #[test]
    fn detects_protocol() {
        let c = WaylandCapabilities {
            connected: true,
            globals: vec!["zwlr_data_control_manager_v1".into()],
        };
        assert!(c.supports_data_control());
    }
}

#[cfg(feature = "wayland")]
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandCapabilities {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
#[cfg(feature = "wayland")]
pub fn has_ext_data_control(caps: &WaylandCapabilities) -> bool {
    caps.globals
        .iter()
        .any(|g| g == "ext_data_control_manager_v1")
}
#[cfg(all(test, feature = "wayland"))]
mod ext_tests {
    use super::*;
    #[test]
    fn ext_detection() {
        let c = WaylandCapabilities {
            connected: true,
            globals: vec!["ext_data_control_manager_v1".into()],
        };
        assert!(has_ext_data_control(&c));
    }
}
#[cfg(feature = "wayland")]
pub struct DataControlProvider {
    pub capabilities: WaylandCapabilities,
}
#[cfg(feature = "wayland")]
impl DataControlProvider {
    pub fn new(capabilities: WaylandCapabilities) -> Option<Self> {
        if has_ext_data_control(&capabilities) {
            Some(Self { capabilities })
        } else {
            None
        }
    }
    pub fn is_bound(&self) -> bool {
        has_ext_data_control(&self.capabilities)
    }
}
#[cfg(all(test, feature = "wayland"))]
mod provider_tests {
    use super::*;
    #[test]
    fn binds_only_when_advertised() {
        let c = WaylandCapabilities {
            connected: true,
            globals: vec!["ext_data_control_manager_v1".into()],
        };
        assert!(DataControlProvider::new(c).unwrap().is_bound());
    }
}
/// Data-control integration boundary. A real event loop must dispatch Wayland
/// events and service offer FDs without blocking the queue; this type documents
/// the required operations while preserving the provider's single-owner rule.
#[cfg(feature = "wayland")]
impl DataControlProvider {
    pub fn read_selection(
        &mut self,
    ) -> Result<Option<crate::protocol::ClipboardProfile>, crate::protocol::ProfileError> {
        Err(crate::protocol::ProfileError::InvalidUtf8)
    }
    pub fn write_selection(
        &mut self,
        _: &crate::protocol::ClipboardProfile,
    ) -> Result<(), crate::protocol::ProfileError> {
        Err(crate::protocol::ProfileError::InvalidUtf8)
    }
}
#[cfg(feature = "wayland")]
pub fn copy_text(text: &str) -> Result<(), String> {
    // wl-copy owns a Wayland data source and remains alive to serve requests;
    // spawning it keeps the HTTP worker nonblocking while preserving lifetime.
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain;charset=utf-8")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn wl-copy: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("wl-copy stdin unavailable")?
        .write_all(text.as_bytes())
        .map_err(|e| format!("write wl-copy: {e}"))?;
    let status = child.wait().map_err(|e| format!("wait wl-copy: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wl-copy exited {status}"))
    }
}

#[cfg(feature = "wayland")]
pub fn paste_text() -> Result<String, String> {
    paste_text_optional()?.ok_or_else(|| "No suitable type of content copied".into())
}

#[cfg(feature = "wayland")]
pub fn paste_text_optional() -> Result<Option<String>, String> {
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, Error, MimeType, Seat};
    let (pipe, _) = match get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
        Ok(contents) => contents,
        Err(Error::ClipboardEmpty | Error::NoMimeType | Error::NoSeats) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    use std::io::Read;
    let mut s = String::new();
    pipe.take((crate::protocol::MAX_TEXT_BYTES + 1) as u64)
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    if s.len() > crate::protocol::MAX_TEXT_BYTES {
        return Ok(None);
    }
    Ok(Some(s))
}
