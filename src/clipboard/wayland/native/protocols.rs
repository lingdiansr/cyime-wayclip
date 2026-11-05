use super::*;
use wayland_client::{
    delegate_noop, event_created_child,
    protocol::{wl_callback, wl_registry, wl_seat},
    Dispatch,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as ext_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as ext_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as ext_source, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as wlr_device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self as wlr_offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as wlr_source, ZwlrDataControlSourceV1},
};

pub(super) enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}
pub(super) enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}
pub(super) enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}
pub(super) enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    pub(super) fn ext(
        registry: &WlRegistry,
        id: u32,
        version: u32,
        qh: &QueueHandle<State>,
    ) -> Self {
        Self::Ext(registry.bind(id, version.min(1), qh, ()))
    }
    pub(super) fn wlr(
        registry: &WlRegistry,
        id: u32,
        version: u32,
        qh: &QueueHandle<State>,
    ) -> Self {
        Self::Wlr(registry.bind(id, version.min(2), qh, ()))
    }
    pub(super) fn protocol(&self) -> &'static str {
        match self {
            Self::Ext(_) => "ext-data-control-v1",
            Self::Wlr(_) => "wlr-data-control-v1",
        }
    }
    pub(super) fn device(&self, seat: &WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Self::Ext(manager) => Device::Ext(manager.get_data_device(seat, qh, ())),
            Self::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, qh, ())),
        }
    }
    pub(super) fn source(&self, qh: &QueueHandle<State>, data: SourceData) -> Source {
        match self {
            Self::Ext(manager) => Source::Ext(manager.create_data_source(qh, data)),
            Self::Wlr(manager) => Source::Wlr(manager.create_data_source(qh, data)),
        }
    }
}
impl Device {
    pub(super) fn select(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(device), Source::Ext(source)) => device.set_selection(Some(source)),
            (Self::Wlr(device), Source::Wlr(source)) => device.set_selection(Some(source)),
            _ => unreachable!("source and device belong to the same manager"),
        }
    }
}
impl Source {
    pub(super) fn offer(&self, mime: &str) {
        match self {
            Self::Ext(source) => source.offer(mime.to_owned()),
            Self::Wlr(source) => source.offer(mime.to_owned()),
        }
    }
    pub(super) fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }
}
impl Offer {
    pub(super) fn id(&self) -> ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }
    pub(super) fn receive(&self, mime: &str, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime.to_owned(), fd),
            Self::Wlr(offer) => offer.receive(mime.to_owned(), fd),
        }
    }
    pub(super) fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                match interface.as_str() {
                    "ext_data_control_manager_v1" if version >= 1 => {
                        state.ext = Some((name, version));
                        if matches!(state.manager, Some(Manager::Wlr(_))) {
                            state.failure =
                                Some("preferred ext data-control manager became available".into());
                        }
                    }
                    "zwlr_data_control_manager_v1" if version >= 1 => {
                        state.wlr = Some((name, version))
                    }
                    "wl_seat" if version >= 1 => {
                        let proxy = registry.bind(name, version.min(7), qh, name);
                        state.seats.insert(name, Seat { proxy, name: None });
                        if state.device.is_some() && state.wanted_seat.is_none() {
                            state.failure = Some(
                                "Wayland seat topology changed; reselecting explicitly".into(),
                            );
                        }
                    }
                    _ => return,
                }
                state.request_configuration(conn, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if state.ext.is_some_and(|(id, _)| id == name) {
                    state.ext = None;
                }
                if state.wlr.is_some_and(|(id, _)| id == name) {
                    state.wlr = None;
                }
                state.seats.remove(&name);
                if state.manager_global == Some(name) || state.selected_seat == Some(name) {
                    state.failure = Some("selected Wayland seat or manager disappeared".into());
                } else {
                    state.request_configuration(conn, qh);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            if let Some(seat) = state.seats.get_mut(global) {
                seat.name = Some(name);
            }
        }
    }
}

impl Dispatch<WlCallback, SyncPoint> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        point: &SyncPoint,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_callback::Event::Done { .. }) {
            return;
        }
        match *point {
            SyncPoint::Registry => {
                state.registry_done = true;
                state.seats_sync = true;
                conn.display().sync(qh, SyncPoint::Seats);
            }
            SyncPoint::Seats => {
                state.seats_sync = false;
                state.configure(conn, qh);
            }
            SyncPoint::Device => {
                state.initialized = true;
                state.shared.update(|published| {
                    published.info.status = ProviderStatus::Ready;
                    published.info.message = "native Wayland clipboard ready".into();
                    if !state.selection_seen {
                        published.pending = false;
                        published.error = Some("compositor sent no initial selection".into());
                    }
                });
            }
            SyncPoint::Write(id) => {
                if let Some(write) = &state.write {
                    if write.id == id {
                        let result = if write.deadline <= Instant::now() {
                            Err("Wayland write confirmation timed out".into())
                        } else if !state.sources.contains_key(&id) {
                            Err("Wayland source cancelled before confirmation".into())
                        } else {
                            Ok(())
                        };
                        state.finish_write(result);
                    }
                }
            }
        }
    }
}

delegate_noop!(State: ignore ExtDataControlManagerV1);
delegate_noop!(State: ignore ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::DataOffer { id } => state.offer(Offer::Ext(id)),
            ext_device::Event::Selection { id } => state.selection(id.map(Offer::Ext)),
            ext_device::Event::PrimarySelection { id } => state.ignore_offer(id.map(Offer::Ext)),
            ext_device::Event::Finished => {
                state.failure = Some("Wayland ext data-control device finished".into())
            }
            _ => {}
        }
    }
    event_created_child!(State, ExtDataControlDeviceV1, [0 => (ExtDataControlOfferV1, ())]);
}
impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: wlr_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::DataOffer { id } => state.offer(Offer::Wlr(id)),
            wlr_device::Event::Selection { id } => state.selection(id.map(Offer::Wlr)),
            wlr_device::Event::PrimarySelection { id } => state.ignore_offer(id.map(Offer::Wlr)),
            wlr_device::Event::Finished => {
                state.failure = Some("Wayland wlr data-control device finished".into())
            }
            _ => {}
        }
    }
    event_created_child!(State, ZwlrDataControlDeviceV1, [0 => (ZwlrDataControlOfferV1, ())]);
}
impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_offer::Event::Offer { mime_type } = event {
            state.mime(offer.id(), mime_type);
        }
    }
}
impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: wlr_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr_offer::Event::Offer { mime_type } = event {
            state.mime(offer.id(), mime_type);
        }
    }
}
impl Dispatch<ExtDataControlSourceV1, SourceData> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_source::Event::Send { mime_type, fd } => state.send(fd, &mime_type, &data.text),
            ext_source::Event::Cancelled => state.cancelled(data.id),
            _ => {}
        }
    }
}
impl Dispatch<ZwlrDataControlSourceV1, SourceData> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: wlr_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_source::Event::Send { mime_type, fd } => state.send(fd, &mime_type, &data.text),
            wlr_source::Event::Cancelled => state.cancelled(data.id),
            _ => {}
        }
    }
}
