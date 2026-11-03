#[cfg(feature = "wayland")]
use wayland_client::{globals::registry_queue_init, Connection};
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
    let Ok((globals, _)) = registry_queue_init::<()>(&conn) else {
        return WaylandCapabilities {
            connected: false,
            globals: vec![],
        };
    };
    WaylandCapabilities {
        connected: true,
        globals: globals
            .contents()
            .iter()
            .map(|g| g.interface.to_string())
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
