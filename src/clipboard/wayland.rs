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
