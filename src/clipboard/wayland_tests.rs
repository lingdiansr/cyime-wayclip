//! Protocol-level tests for the native data-control clipboard backend.
//!
//! Every test owns a minimal compositor: a `wayland_server::Display` driven by a
//! dedicated thread over a real `UnixStream` pair, with SCM_RIGHTS transfers for
//! clipboard data. The backend is injected through `start_with_connector`, so no
//! test reads or mutates the user's Wayland socket or environment.
//!
//! The compositor only implements what the clipboard protocol needs: `wl_seat`,
//! the `ext-data-control-v1` and `wlr-data-control-unstable-v1` managers, data
//! devices, sources and offers. Data delivery happens on the same socket pair
//! the backend would use with a real compositor.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wayland_protocols::ext::data_control::v1::server::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_server::{
    backend::ClientData, protocol::wl_seat, Client, DataInit, Dispatch, Display, DisplayHandle,
    GlobalDispatch, New, Resource,
};

use super::super::{ClipboardSnapshot, ProviderStatus};
use super::{start_with_connector, BackendInfo, WaylandClipboard};

/// MIME types the backend is required to advertise when it becomes the owner.
const EXPECTED_MIMES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];

const POLL: Duration = Duration::from_millis(2);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Compositor specification and observable state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Selection {
    mimes: Vec<String>,
    data: Vec<u8>,
}

fn text_selection(text: &str) -> Selection {
    Selection {
        mimes: EXPECTED_MIMES
            .iter()
            .map(|mime| (*mime).to_owned())
            .collect(),
        data: text.as_bytes().to_vec(),
    }
}

#[derive(Clone)]
struct ServerSpec {
    seats: Vec<String>,
    ext: bool,
    wlr: bool,
    initial: Option<Selection>,
}

impl ServerSpec {
    fn new(ext: bool, wlr: bool, initial: Option<Selection>) -> Self {
        ServerSpec {
            seats: vec!["seat0".into()],
            ext,
            wlr,
            initial,
        }
    }
}

/// State the compositor recorded, asserted by tests from the test thread.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    connected: bool,
    ext_manager: bool,
    wlr_manager: bool,
    ext_device: bool,
    wlr_device: bool,
    device_seat: Option<String>,
    seat_names: Vec<String>,
    source_mimes: Vec<String>,
    sources: usize,
    selections: usize,
    nulls: usize,
    receives: Vec<String>,
}

// ---------------------------------------------------------------------------
// Compositor objects
// ---------------------------------------------------------------------------

struct SeatGlobal {
    name: String,
}

struct SeatName(String);

#[derive(Clone)]
struct OfferData {
    data: Arc<Vec<u8>>,
    stalled: bool,
}

#[derive(Clone)]
enum DeviceRes {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ext,
    Wlr,
}

impl DeviceRes {
    fn kind(&self) -> Kind {
        match self {
            DeviceRes::Ext(_) => Kind::Ext,
            DeviceRes::Wlr(_) => Kind::Wlr,
        }
    }

    fn client(&self) -> Option<Client> {
        match self {
            DeviceRes::Ext(device) => device.client(),
            DeviceRes::Wlr(device) => device.client(),
        }
    }

    fn data_offer(&self, offer: &OfferRes) {
        match (self, offer) {
            (DeviceRes::Ext(device), OfferRes::Ext(offer)) => device.data_offer(offer),
            (DeviceRes::Wlr(device), OfferRes::Wlr(offer)) => device.data_offer(offer),
            _ => {}
        }
    }

    fn selection(&self, offer: Option<&OfferRes>) {
        match (self, offer) {
            (DeviceRes::Ext(device), Some(OfferRes::Ext(offer))) => device.selection(Some(offer)),
            (DeviceRes::Ext(device), None) => device.selection(None),
            (DeviceRes::Wlr(device), Some(OfferRes::Wlr(offer))) => device.selection(Some(offer)),
            (DeviceRes::Wlr(device), None) => device.selection(None),
            _ => {}
        }
    }

    fn finished(&self) {
        match self {
            DeviceRes::Ext(device) => device.finished(),
            DeviceRes::Wlr(device) => device.finished(),
        }
    }
}

enum OfferRes {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl OfferRes {
    fn offer(&self, mime_type: String) {
        match self {
            OfferRes::Ext(offer) => offer.offer(mime_type),
            OfferRes::Wlr(offer) => offer.offer(mime_type),
        }
    }
}

#[derive(Clone)]
enum SourceRes {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl SourceRes {
    fn send(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            SourceRes::Ext(source) => source.send(mime_type, fd),
            SourceRes::Wlr(source) => source.send(mime_type, fd),
        }
    }

    fn cancelled(&self) {
        match self {
            SourceRes::Ext(source) => source.cancelled(),
            SourceRes::Wlr(source) => source.cancelled(),
        }
    }
}

/// A selection the compositor still has to deliver. `data == None` unsets it.
struct PendingSelection {
    mimes: Vec<String>,
    data: Option<Arc<Vec<u8>>>,
    stalled: bool,
}

struct ServerState {
    shared: Arc<Mutex<Snapshot>>,
    device: Option<DeviceRes>,
    pending: Option<PendingSelection>,
    /// Source the client installed through `set_selection`.
    source: Option<SourceRes>,
    /// Receive file descriptors deliberately left without data, for stall tests.
    held: Vec<OwnedFd>,
    seat_globals: Vec<wayland_server::backend::GlobalId>,
}

fn record(state: &ServerState) -> std::sync::MutexGuard<'_, Snapshot> {
    state.shared.lock().unwrap()
}

// ---------------------------------------------------------------------------
// Selection delivery
// ---------------------------------------------------------------------------

fn advertise(state: &mut ServerState, handle: &DisplayHandle) {
    let Some(device) = state.device.clone() else {
        // A selection offered before the device exists is delivered on binding.
        return;
    };
    let Some(pending) = state.pending.take() else {
        return;
    };
    let Some(client) = device.client() else {
        return;
    };
    let Some(data) = pending.data else {
        device.selection(None);
        record(state).nulls += 1;
        return;
    };

    let offer_data = OfferData {
        data,
        stalled: pending.stalled,
    };
    let offer = match device.kind() {
        Kind::Ext => client
            .create_resource::<ExtDataControlOfferV1, OfferData, ServerState>(handle, 1, offer_data)
            .ok()
            .map(OfferRes::Ext),
        Kind::Wlr => client
            .create_resource::<ZwlrDataControlOfferV1, OfferData, ServerState>(
                handle, 1, offer_data,
            )
            .ok()
            .map(OfferRes::Wlr),
    };
    let Some(offer) = offer else {
        return;
    };
    device.data_offer(&offer);
    for mime in &pending.mimes {
        offer.offer(mime.clone());
    }
    device.selection(Some(&offer));
}

fn handle_receive(state: &mut ServerState, mime_type: String, fd: OwnedFd, data: &OfferData) {
    record(state).receives.push(mime_type);
    if data.stalled {
        state.held.push(fd);
    } else {
        let mut sink = std::fs::File::from(fd);
        let _ = sink.write_all(&data.data);
    }
}

// ---------------------------------------------------------------------------
// Dispatch: wl_seat
// ---------------------------------------------------------------------------

impl GlobalDispatch<wl_seat::WlSeat, SeatGlobal> for ServerState {
    fn bind(
        state: &mut ServerState,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        global_data: &SeatGlobal,
        data_init: &mut DataInit<'_, ServerState>,
    ) {
        let seat = data_init.init(resource, SeatName(global_data.name.clone()));
        if seat.version() >= 2 {
            seat.name(global_data.name.clone());
            record(state).seat_names.push(global_data.name.clone());
        }
        seat.capabilities(wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<wl_seat::WlSeat, SeatName> for ServerState {
    fn request(
        _state: &mut ServerState,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &SeatName,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let wl_seat::Request::Release = request {}
    }
}

// ---------------------------------------------------------------------------
// Dispatch: ext-data-control-v1
// ---------------------------------------------------------------------------

impl GlobalDispatch<ExtDataControlManagerV1, ()> for ServerState {
    fn bind(
        state: &mut ServerState,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtDataControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, ServerState>,
    ) {
        data_init.init(resource, ());
        record(state).ext_manager = true;
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ExtDataControlManagerV1,
        request: ext_data_control_manager_v1::Request,
        _data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, ServerState>,
    ) {
        match request {
            ext_data_control_manager_v1::Request::CreateDataSource { id } => {
                data_init.init(id, ());
                let mut snapshot = record(state);
                snapshot.sources += 1;
                snapshot.source_mimes.clear();
            }
            ext_data_control_manager_v1::Request::GetDataDevice { id, seat } => {
                let seat_name = seat.data::<SeatName>().map(|name| name.0.clone());
                let device = data_init.init(id, ());
                {
                    let mut snapshot = record(state);
                    snapshot.ext_device = true;
                    snapshot.device_seat = seat_name;
                }
                state.device = Some(DeviceRes::Ext(device));
                advertise(state, handle);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ExtDataControlSourceV1,
        request: ext_data_control_source_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let ext_data_control_source_v1::Request::Offer { mime_type } = request {
            record(state).source_mimes.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ExtDataControlDeviceV1,
        request: ext_data_control_device_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let ext_data_control_device_v1::Request::SetSelection { source } = request {
            install_source(state, source.map(SourceRes::Ext));
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, OfferData> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ExtDataControlOfferV1,
        request: ext_data_control_offer_v1::Request,
        data: &OfferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let ext_data_control_offer_v1::Request::Receive { mime_type, fd } = request {
            handle_receive(state, mime_type, fd, data);
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch: wlr-data-control-unstable-v1
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZwlrDataControlManagerV1, ()> for ServerState {
    fn bind(
        state: &mut ServerState,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrDataControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, ServerState>,
    ) {
        data_init.init(resource, ());
        record(state).wlr_manager = true;
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ZwlrDataControlManagerV1,
        request: zwlr_data_control_manager_v1::Request,
        _data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, ServerState>,
    ) {
        match request {
            zwlr_data_control_manager_v1::Request::CreateDataSource { id } => {
                data_init.init(id, ());
                let mut snapshot = record(state);
                snapshot.sources += 1;
                snapshot.source_mimes.clear();
            }
            zwlr_data_control_manager_v1::Request::GetDataDevice { id, seat } => {
                let seat_name = seat.data::<SeatName>().map(|name| name.0.clone());
                let device = data_init.init(id, ());
                {
                    let mut snapshot = record(state);
                    snapshot.wlr_device = true;
                    snapshot.device_seat = seat_name;
                }
                state.device = Some(DeviceRes::Wlr(device));
                advertise(state, handle);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ZwlrDataControlSourceV1,
        request: zwlr_data_control_source_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let zwlr_data_control_source_v1::Request::Offer { mime_type } = request {
            record(state).source_mimes.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ZwlrDataControlDeviceV1,
        request: zwlr_data_control_device_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let zwlr_data_control_device_v1::Request::SetSelection { source } = request {
            install_source(state, source.map(SourceRes::Wlr));
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, OfferData> for ServerState {
    fn request(
        state: &mut ServerState,
        _client: &Client,
        _resource: &ZwlrDataControlOfferV1,
        request: zwlr_data_control_offer_v1::Request,
        data: &OfferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, ServerState>,
    ) {
        if let zwlr_data_control_offer_v1::Request::Receive { mime_type, fd } = request {
            handle_receive(state, mime_type, fd, data);
        }
    }
}

fn install_source(state: &mut ServerState, source: Option<SourceRes>) {
    let null = source.is_none();
    if let Some(previous) = state.source.take() {
        previous.cancelled();
    }
    state.source = source;
    let mut snapshot = record(state);
    snapshot.selections += 1;
    if null {
        snapshot.nulls += 1;
    }
}

// ---------------------------------------------------------------------------
// Compositor thread
// ---------------------------------------------------------------------------

struct TestClientData;

impl ClientData for TestClientData {}

enum Command {
    Offer(Selection),
    OfferStalled,
    Clear,
    ReleaseHeld(Option<Vec<u8>>),
    Fetch {
        mime: String,
        reply: mpsc::Sender<Result<Vec<u8>, String>>,
    },
    HoldPaste(mpsc::Sender<std::fs::File>),
    RemoveSeat,
    FinishDevice,
    Shutdown,
}

fn serve(
    stream: UnixStream,
    spec: ServerSpec,
    shared: Arc<Mutex<Snapshot>>,
    receiver: mpsc::Receiver<Command>,
    stop: Arc<AtomicBool>,
) {
    let mut display = match Display::<ServerState>::new() {
        Ok(display) => display,
        Err(_) => return,
    };
    let mut state = ServerState {
        shared: Arc::clone(&shared),
        device: None,
        pending: spec.initial.as_ref().map(|selection| PendingSelection {
            mimes: selection.mimes.clone(),
            data: Some(Arc::new(selection.data.clone())),
            stalled: false,
        }),
        source: None,
        held: Vec::new(),
        seat_globals: Vec::new(),
    };
    {
        let handle = display.handle();
        for name in &spec.seats {
            let global = handle.create_global::<ServerState, wl_seat::WlSeat, SeatGlobal>(
                9,
                SeatGlobal { name: name.clone() },
            );
            state.seat_globals.push(global);
        }
        if spec.ext {
            handle.create_global::<ServerState, ExtDataControlManagerV1, ()>(1, ());
        }
        if spec.wlr {
            handle.create_global::<ServerState, ZwlrDataControlManagerV1, ()>(2, ());
        }
    }
    shared.lock().unwrap().connected = true;
    if display
        .handle()
        .insert_client(stream, Arc::new(TestClientData))
        .is_err()
    {
        shared.lock().unwrap().connected = false;
        return;
    }

    loop {
        if display.dispatch_clients(&mut state).is_err() {
            break;
        }
        if display.flush_clients().is_err() {
            break;
        }

        let mut terminate = false;
        loop {
            match receiver.try_recv() {
                Ok(command) => {
                    if apply(&mut state, &mut display, command) {
                        terminate = true;
                        break;
                    }
                    if display.flush_clients().is_err() {
                        terminate = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    terminate = true;
                    break;
                }
            }
        }
        if terminate || stop.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_micros(200));
    }

    shared.lock().unwrap().connected = false;
    drop(display);
}

/// Returns `true` when the compositor thread should stop.
fn apply(state: &mut ServerState, display: &mut Display<ServerState>, command: Command) -> bool {
    match command {
        Command::Offer(selection) => {
            state.pending = Some(PendingSelection {
                mimes: selection.mimes,
                data: Some(Arc::new(selection.data)),
                stalled: false,
            });
            advertise(state, &display.handle());
        }
        Command::OfferStalled => {
            state.pending = Some(PendingSelection {
                mimes: EXPECTED_MIMES
                    .iter()
                    .map(|mime| (*mime).to_owned())
                    .collect(),
                data: Some(Arc::new(Vec::new())),
                stalled: true,
            });
            advertise(state, &display.handle());
        }
        Command::Clear => {
            state.pending = Some(PendingSelection {
                mimes: Vec::new(),
                data: None,
                stalled: false,
            });
            advertise(state, &display.handle());
        }
        Command::ReleaseHeld(payload) => {
            for fd in state.held.drain(..) {
                if let Some(bytes) = &payload {
                    let mut sink = std::fs::File::from(fd);
                    let _ = sink.write_all(bytes);
                }
            }
        }
        Command::Fetch { mime, reply } => {
            let result = fetch_from_source(state, display, &mime);
            let _ = reply.send(result);
        }
        Command::HoldPaste(reply) => {
            let mut fds = [-1; 2];
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } >= 4096);
            state
                .source
                .as_ref()
                .expect("source must exist")
                .send("text/plain".into(), writer.as_fd());
            drop(writer);
            let _ = reply.send(reader);
        }
        Command::RemoveSeat => {
            let global = state.seat_globals.pop().expect("seat must exist");
            display.handle().disable_global::<ServerState>(global);
        }
        Command::FinishDevice => {
            state.device.as_ref().expect("device must exist").finished();
        }
        Command::Shutdown => return true,
    }
    false
}

/// Acts as the pasting client: asks the client's own source for `mime` and reads
/// what the backend writes into the transfer pipe.
fn fetch_from_source(
    state: &mut ServerState,
    display: &mut Display<ServerState>,
    mime: &str,
) -> Result<Vec<u8>, String> {
    let Some(source) = state.source.clone() else {
        return Err("client did not install a source".into());
    };
    let (write_end, mut read_end) = UnixStream::pair().map_err(|error| error.to_string())?;
    source.send(mime.to_owned(), write_end.as_fd());
    drop(write_end);
    display.flush_clients().map_err(|error| error.to_string())?;
    read_end
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    read_end
        .read_to_end(&mut bytes)
        .map_err(|error| format!("source did not serve {mime}: {error}"))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Factory / harness
// ---------------------------------------------------------------------------

struct Server {
    shared: Arc<Mutex<Snapshot>>,
    commands: mpsc::Sender<Command>,
    join: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Factory {
    specs: VecDeque<ServerSpec>,
    last: Option<ServerSpec>,
    servers: Vec<Server>,
}

impl Factory {
    fn new(spec: ServerSpec) -> Self {
        Factory {
            specs: VecDeque::from(vec![spec]),
            last: None,
            servers: Vec::new(),
        }
    }

    /// Queue the specification of the next compositor connection.
    fn queue(&mut self, spec: ServerSpec) {
        self.specs.push_back(spec);
    }

    fn connect(&mut self) -> Result<wayland_client::Connection, String> {
        let (client_end, server_end) = UnixStream::pair().map_err(|error| error.to_string())?;
        let spec = self
            .specs
            .pop_front()
            .or_else(|| self.last.clone())
            .ok_or_else(|| "no compositor specification left".to_string())?;
        self.last = Some(spec.clone());
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let (commands, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("test-compositor".into())
            .spawn(move || serve(server_end, spec, thread_shared, receiver, thread_stop))
            .map_err(|error| error.to_string())?;
        self.servers.push(Server {
            shared,
            commands,
            join: Some(join),
            stop,
        });
        wayland_client::Connection::from_socket(client_end)
            .map_err(|error| format!("client socket setup failed: {error}"))
    }

    fn latest(&self) -> &Server {
        self.servers
            .last()
            .expect("no compositor connection was made")
    }

    fn snapshot(&self) -> Snapshot {
        self.latest().shared.lock().unwrap().clone()
    }

    fn send(&self, command: Command) {
        let _ = self.latest().commands.send(command);
    }

    /// Stop the most recent compositor and wait until its socket is closed.
    fn shutdown_latest(&mut self) {
        if let Some(server) = self.servers.last_mut() {
            let _ = server.commands.send(Command::Shutdown);
            server.stop.store(true, Ordering::Release);
            if let Some(join) = server.join.take() {
                let _ = join.join();
            }
        }
    }
}

struct Launched {
    clip: WaylandClipboard,
    factory: Arc<Mutex<Factory>>,
}

impl Launched {
    fn snapshot(&self) -> Snapshot {
        self.factory.lock().unwrap().snapshot()
    }

    fn send(&self, command: Command) {
        self.factory.lock().unwrap().send(command);
    }

    fn fetch(&self, mime: &str) -> Result<Vec<u8>, String> {
        let (reply, result) = mpsc::channel();
        self.send(Command::Fetch {
            mime: mime.to_owned(),
            reply,
        });
        result
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| format!("no source reply: {error}"))?
    }
}

fn try_launch(
    seat: Option<&str>,
    spec: ServerSpec,
) -> (std::io::Result<WaylandClipboard>, Arc<Mutex<Factory>>) {
    let factory = Arc::new(Mutex::new(Factory::new(spec)));
    let connector = Arc::clone(&factory);
    let result = start_with_connector(seat.map(str::to_owned), move || {
        connector.lock().unwrap().connect()
    });
    (result, factory)
}

fn launch(seat: Option<&str>, spec: ServerSpec) -> Launched {
    let (result, factory) = try_launch(seat, spec);
    Launched {
        clip: result.expect("backend construction failed"),
        factory,
    }
}

// ---------------------------------------------------------------------------
// Waiting helpers
// ---------------------------------------------------------------------------

fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

fn wait_ready(clip: &WaylandClipboard, timeout: Duration) -> BackendInfo {
    let deadline = Instant::now() + timeout;
    loop {
        let info = clip.info();
        if info.status == ProviderStatus::Ready {
            return info;
        }
        if Instant::now() >= deadline {
            panic!("backend never became Ready: {info:?}");
        }
        thread::sleep(POLL);
    }
}

fn wait_status(clip: &WaylandClipboard, status: ProviderStatus, timeout: Duration) -> BackendInfo {
    assert!(
        wait_for(timeout, || clip.info().status == status),
        "backend never reported {status:?}: {:?}",
        clip.info()
    );
    clip.info()
}

fn wait_read(clip: &WaylandClipboard, expected: &str, timeout: Duration) -> ClipboardSnapshot {
    let deadline = Instant::now() + timeout;
    let short: String = expected.chars().take(48).collect();
    loop {
        let outcome = clip.read();
        if matches!(&outcome, Ok(snapshot) if snapshot.text.as_deref() == Some(expected)) {
            return outcome.expect("matched a successful read");
        }
        if Instant::now() >= deadline {
            panic!(
                "selection {short:?} ({} bytes) was not observed, last result {outcome:?}",
                expected.len()
            );
        }
        thread::sleep(POLL);
    }
}

fn wait_empty(clip: &WaylandClipboard, timeout: Duration) -> ClipboardSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let outcome = clip.read();
        if matches!(&outcome, Ok(snapshot) if snapshot.text.is_none()) {
            return outcome.expect("matched a successful read");
        }
        if Instant::now() >= deadline {
            panic!("empty selection was not observed, last result {outcome:?}");
        }
        thread::sleep(POLL);
    }
}

/// Waits for an empty selection that is newer than `previous`, so a stale
/// snapshot from a previous step cannot satisfy the wait.
fn wait_empty_newer(
    clip: &WaylandClipboard,
    previous: u64,
    timeout: Duration,
) -> ClipboardSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let outcome = clip.read();
        if matches!(&outcome, Ok(snapshot) if snapshot.text.is_none() && snapshot.generation > previous)
        {
            return outcome.expect("matched a successful read");
        }
        if Instant::now() >= deadline {
            panic!("newer empty selection was not observed, last result {outcome:?}");
        }
        thread::sleep(POLL);
    }
}

fn sorted_mimes(mimes: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = mimes.iter().map(|mime| mime.to_ascii_lowercase()).collect();
    unique.sort();
    unique.dedup();
    unique
}

// ---------------------------------------------------------------------------
// Manager selection and seat resolution
// ---------------------------------------------------------------------------

#[test]
fn ext_data_control_is_preferred_when_both_managers_are_advertised() {
    let spec = ServerSpec::new(true, true, Some(text_selection("both-managers")));
    let app = launch(None, spec);

    let info = wait_ready(&app.clip, READY_TIMEOUT);
    assert_eq!(info.protocol, Some("ext-data-control-v1"));
    assert_eq!(info.status, ProviderStatus::Ready);
    assert_eq!(info.seat.as_deref(), Some("seat0"));

    let snapshot = app.snapshot();
    assert!(snapshot.connected, "compositor never accepted the client");
    assert!(snapshot.ext_manager, "ext manager was not bound");
    assert!(snapshot.ext_device, "no ext data device was created");
    assert!(
        !snapshot.wlr_device,
        "wlr device was used despite ext priority"
    );
    assert_eq!(snapshot.device_seat.as_deref(), Some("seat0"));

    let read = wait_read(&app.clip, "both-managers", READ_TIMEOUT);
    assert_eq!(read.text.as_deref(), Some("both-managers"));
}

#[test]
fn wlr_data_control_is_used_when_ext_is_absent() {
    let spec = ServerSpec::new(false, true, Some(text_selection("from-wlr")));
    let app = launch(None, spec);

    let info = wait_ready(&app.clip, READY_TIMEOUT);
    assert_eq!(info.protocol, Some("wlr-data-control-v1"));
    let snapshot = app.snapshot();
    assert!(snapshot.wlr_device, "no wlr data device was created");
    assert!(
        !snapshot.ext_device,
        "ext device was created without an ext manager"
    );
    assert_eq!(snapshot.device_seat.as_deref(), Some("seat0"));

    let read = wait_read(&app.clip, "from-wlr", READ_TIMEOUT);
    app.clip
        .write("wlr-write", read.generation)
        .expect("write through wlr data device");
    assert!(wait_for(Duration::from_secs(2), || app
        .snapshot()
        .selections
        == 1));
    assert_eq!(
        app.fetch("UTF8_STRING").expect("wlr source"),
        b"wlr-write".to_vec()
    );
}

#[test]
fn missing_data_control_is_reported_as_unsupported() {
    let (result, factory) = try_launch(None, ServerSpec::new(false, false, None));
    let clip = result.expect("unsupported backend must still yield a handle");
    wait_status(&clip, ProviderStatus::Unsupported, READY_TIMEOUT);
    let info = clip.info();
    assert_eq!(info.protocol, None);
    assert!(clip.read().is_err(), "unsupported backend must not read");
    assert!(
        clip.write("text", 0).is_err(),
        "unsupported backend must not write"
    );
    drop(factory);
}

#[test]
fn ambiguous_multiple_seats_are_rejected_without_explicit_choice() {
    let mut spec = ServerSpec::new(true, false, Some(text_selection("seated")));
    spec.seats = vec!["seat-primary".into(), "seat-secondary".into()];
    let (result, factory) = try_launch(None, spec);
    let clip = result.expect("ambiguous backend must still yield a handle");
    wait_status(&clip, ProviderStatus::Unsupported, READY_TIMEOUT);
    assert_eq!(clip.info().protocol, None);
    assert!(clip.read().is_err());
    drop(factory);
}

#[test]
fn explicit_seat_choice_binds_that_seat() {
    let mut spec = ServerSpec::new(true, false, Some(text_selection("seat-specific")));
    spec.seats = vec!["seat-primary".into(), "seat-secondary".into()];
    let app = launch(Some("seat-secondary"), spec);

    wait_ready(&app.clip, READY_TIMEOUT);
    let snapshot = app.snapshot();
    assert_eq!(snapshot.device_seat.as_deref(), Some("seat-secondary"));
    assert!(
        snapshot
            .seat_names
            .iter()
            .any(|name| name == "seat-secondary"),
        "selected seat name was never delivered: {:?}",
        snapshot.seat_names
    );
    let read = wait_read(&app.clip, "seat-specific", READ_TIMEOUT);
    assert_eq!(read.text.as_deref(), Some("seat-specific"));
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[test]
fn reads_external_utf8_with_emoji_and_crlf_verbatim() {
    let text = "clipboard 😀🦀\r\nsecond line\r\n";
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection(text))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);

    let snapshot = wait_read(&app.clip, text, READ_TIMEOUT);
    assert_eq!(snapshot.text.as_deref(), Some(text));
}

#[test]
fn null_and_non_text_selections_clear_text_and_advance_generation() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("first"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    let first = wait_read(&app.clip, "first", READ_TIMEOUT);

    app.send(Command::Clear);
    let null = wait_empty(&app.clip, READ_TIMEOUT);
    assert_eq!(
        app.snapshot().nulls,
        1,
        "compositor NULL selection was not delivered"
    );
    assert!(
        null.generation > first.generation,
        "NULL selection must advance the generation: {} -> {}",
        first.generation,
        null.generation
    );

    app.send(Command::Offer(Selection {
        mimes: vec!["application/octet-stream".into()],
        data: vec![0x00, 0x01, 0x02, 0xff],
    }));
    let non_text = wait_empty_newer(&app.clip, null.generation, READ_TIMEOUT);
    assert!(
        non_text.generation > null.generation,
        "non-text selection must advance the generation: {} -> {}",
        null.generation,
        non_text.generation
    );

    app.send(Command::Offer(text_selection("recovered")));
    assert_eq!(
        wait_read(&app.clip, "recovered", READ_TIMEOUT)
            .text
            .as_deref(),
        Some("recovered")
    );
}

#[test]
fn enforces_size_and_utf8_boundaries_for_incoming_text() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    let seed = wait_read(&app.clip, "seed", READ_TIMEOUT);

    // Exactly MAX_TEXT_BYTES is accepted verbatim.
    let exact = "a".repeat(65_536);
    app.send(Command::Offer(text_selection(&exact)));
    let snapshot = wait_read(&app.clip, &exact, READ_TIMEOUT);
    assert_eq!(snapshot.text.as_ref().map(|text| text.len()), Some(65_536));
    assert!(snapshot.generation > seed.generation);

    // One byte over the limit is rejected, not truncated and not cached.
    let oversized = "b".repeat(65_537);
    app.send(Command::Offer(text_selection(&oversized)));
    assert!(
        wait_for(READ_TIMEOUT, || {
            !matches!(app.clip.read(), Ok(snapshot) if snapshot.text.as_deref() == Some(exact.as_str()))
        }),
        "oversized selection never replaced the previous snapshot"
    );
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        if let Ok(snapshot) = app.clip.read() {
            assert_ne!(
                snapshot.text.as_ref().map(|text| text.len()),
                Some(oversized.len()),
                "oversized transfer was cached"
            );
            if snapshot.text.is_some() {
                panic!("oversized transfer produced text: {snapshot:?}");
            }
        }
        thread::sleep(POLL);
    }
    assert!(
        app.clip.read().is_err(),
        "oversized transfer must end in an error"
    );

    app.send(Command::Offer(text_selection("after-oversize")));
    assert_eq!(
        wait_read(&app.clip, "after-oversize", READ_TIMEOUT)
            .text
            .as_deref(),
        Some("after-oversize")
    );

    // Invalid UTF-8 is rejected as well.
    app.send(Command::Offer(Selection {
        mimes: EXPECTED_MIMES
            .iter()
            .map(|mime| (*mime).to_owned())
            .collect(),
        data: b"broken \xff\xfe".to_vec(),
    }));
    assert!(
        wait_for(READ_TIMEOUT, || {
            !matches!(app.clip.read(), Ok(snapshot) if snapshot.text.as_deref() == Some("after-oversize"))
        }),
        "invalid UTF-8 selection never replaced the previous snapshot"
    );
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        if let Ok(snapshot) = app.clip.read() {
            assert!(
                snapshot.text.is_none(),
                "invalid UTF-8 produced text: {snapshot:?}"
            );
        }
        thread::sleep(POLL);
    }
    assert!(
        app.clip.read().is_err(),
        "invalid UTF-8 must end in an error"
    );

    app.send(Command::Offer(text_selection("after-invalid")));
    assert_eq!(
        wait_read(&app.clip, "after-invalid", READ_TIMEOUT)
            .text
            .as_deref(),
        Some("after-invalid")
    );
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[test]
fn write_advertises_every_mime_type_and_serves_exact_bytes_repeatedly() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    let seed = wait_read(&app.clip, "seed", READ_TIMEOUT);

    let text = "written \u{1F980} \u{4F60}\u{597D}\r\n";
    app.clip
        .write(text, seed.generation)
        .expect("write is accepted");

    assert!(
        wait_for(Duration::from_secs(2), || app.snapshot().selections == 1),
        "compositor never saw set_selection"
    );
    let snapshot = app.snapshot();
    assert_eq!(
        snapshot.sources, 1,
        "write must use exactly one native source"
    );

    let advertised = sorted_mimes(&snapshot.source_mimes);
    let mut expected: Vec<String> = EXPECTED_MIMES
        .iter()
        .map(|mime| mime.to_ascii_lowercase())
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(
        advertised, expected,
        "write advertised an unexpected MIME set"
    );

    for mime in EXPECTED_MIMES {
        let bytes = app
            .fetch(mime)
            .unwrap_or_else(|error| panic!("{mime}: {error}"));
        assert_eq!(bytes, text.as_bytes(), "MIME {mime} served different bytes");
    }

    // Pasting the same source twice must serve identical bytes both times.
    let first = app.fetch("text/plain;charset=utf-8").expect("first paste");
    let second = app.fetch("text/plain;charset=utf-8").expect("second paste");
    assert_eq!(first, text.as_bytes());
    assert_eq!(second, first);
}

#[test]
fn write_rejects_invalid_text_without_consuming_the_generation() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    let seed = wait_read(&app.clip, "seed", READ_TIMEOUT);

    let cases = [
        ("empty", String::new()),
        ("NUL", "a\0b".to_string()),
        ("oversized", "x".repeat(65_537)),
    ];
    for (what, text) in cases {
        assert!(
            app.clip.write(&text, seed.generation).is_err(),
            "{what} text must be rejected"
        );
    }
    assert_eq!(
        app.snapshot().selections,
        0,
        "rejected writes replaced the selection"
    );
    assert_eq!(
        app.clip
            .read()
            .expect("selection intact after rejected writes")
            .text
            .as_deref(),
        Some("seed")
    );

    // Rejected attempts must not consume the generation the caller observed.
    app.clip
        .write("accepted", seed.generation)
        .expect("valid write works after rejected attempts");
    assert!(wait_for(Duration::from_secs(2), || app
        .snapshot()
        .selections
        == 1));
    assert_eq!(
        app.fetch("text/plain").expect("accepted source"),
        b"accepted".to_vec()
    );
}

// ---------------------------------------------------------------------------
// Stalls, replacement and conditional writes
// ---------------------------------------------------------------------------

#[test]
fn stalled_transfer_does_not_freeze_dispatch_and_recovers_after_timeout() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    wait_read(&app.clip, "seed", READ_TIMEOUT);

    let before = app.snapshot().receives.len();
    app.send(Command::OfferStalled);
    assert!(
        wait_for(Duration::from_secs(2), || app.snapshot().receives.len()
            > before),
        "backend never requested the stalled selection"
    );
    let requested_at = Instant::now();

    // The pending transfer must not block command dispatch: an always-invalid
    // generation is rejected immediately rather than after WRITE_TIMEOUT.
    let started = Instant::now();
    let rejected = app.clip.write("cannot-apply", u64::MAX);
    assert!(rejected.is_err(), "write with a stale generation must fail");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "write dispatch was blocked behind a stalled read for {elapsed:?}"
    );
    assert_eq!(app.clip.info().status, ProviderStatus::Ready);

    // Past IO_TIMEOUT the transfer fails and the selection stays unusable.
    let remaining = Duration::from_millis(2_500).saturating_sub(requested_at.elapsed());
    thread::sleep(remaining);
    assert!(
        app.clip.read().is_err(),
        "timed out transfer must not expose text"
    );

    let selections = app.snapshot().selections;
    app.send(Command::Offer(text_selection("post-stall")));
    let recovered = wait_read(&app.clip, "post-stall", READ_TIMEOUT);

    // A write still works after the stalled transfer was abandoned.
    app.clip
        .write("after-stall", recovered.generation)
        .expect("write after timeout");
    assert!(wait_for(Duration::from_secs(2), || app
        .snapshot()
        .selections
        == selections + 1));
    assert_eq!(
        app.fetch("text/plain;charset=utf-8")
            .expect("source after stall"),
        b"after-stall".to_vec()
    );
}

#[test]
fn late_data_from_a_replaced_offer_cannot_overwrite_the_newer_selection() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    let seed = wait_read(&app.clip, "seed", READ_TIMEOUT);

    let before = app.snapshot().receives.len();
    app.send(Command::OfferStalled);
    assert!(
        wait_for(Duration::from_secs(2), || app.snapshot().receives.len()
            > before),
        "backend never requested the stalled selection"
    );

    app.send(Command::Offer(text_selection("newer")));
    let newer = wait_read(&app.clip, "newer", READ_TIMEOUT);
    assert!(
        newer.generation > seed.generation,
        "newer selection must advance the generation: {} -> {}",
        seed.generation,
        newer.generation
    );

    // Late bytes belonging to the replaced offer arrive after the newer read.
    app.send(Command::ReleaseHeld(Some(b"late-old".to_vec())));
    thread::sleep(Duration::from_millis(300));
    let after = app
        .clip
        .read()
        .expect("newer selection must remain readable");
    assert_eq!(
        after.text.as_deref(),
        Some("newer"),
        "late data replaced the newer selection"
    );
    assert_eq!(
        after.generation, newer.generation,
        "late data changed the generation"
    );

    // A mismatched expected generation is rejected without touching the selection.
    let selections = app.snapshot().selections;
    assert!(
        app.clip
            .write("stale-write", newer.generation.wrapping_add(1))
            .is_err(),
        "write with a mismatched generation must fail"
    );
    assert_eq!(
        app.snapshot().selections,
        selections,
        "failed write replaced the selection"
    );
    assert_eq!(
        app.clip.read().expect("selection intact").text.as_deref(),
        Some("newer")
    );

    // The current generation is still writable.
    app.clip
        .write("fresh-write", newer.generation)
        .expect("write with current generation");
    assert!(wait_for(Duration::from_secs(2), || app
        .snapshot()
        .selections
        == selections + 1));
    assert_eq!(
        app.fetch("text/plain").expect("current source"),
        b"fresh-write".to_vec()
    );
}

#[test]
fn shutting_down_does_not_wait_for_a_pending_transfer() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    wait_ready(&app.clip, READY_TIMEOUT);
    wait_read(&app.clip, "seed", READ_TIMEOUT);

    let before = app.snapshot().receives.len();
    app.send(Command::OfferStalled);
    assert!(
        wait_for(Duration::from_secs(2), || app.snapshot().receives.len()
            > before),
        "backend never requested the stalled selection"
    );

    // Dropping the handle tears down the worker; a pending transfer must not
    // hold shutdown for its IO deadline.
    let started = Instant::now();
    drop(app);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown blocked behind a pending transfer for {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Disconnects
// ---------------------------------------------------------------------------

#[test]
fn reconnect_invalidates_cached_text_and_does_not_replay_the_old_write() {
    let first_spec = ServerSpec::new(true, false, Some(text_selection("one")));
    let second_spec = ServerSpec::new(true, false, Some(text_selection("two")));
    let factory = Arc::new(Mutex::new(Factory::new(first_spec)));
    factory.lock().unwrap().queue(second_spec);
    let connector = Arc::clone(&factory);
    let clip = start_with_connector(None, move || connector.lock().unwrap().connect())
        .expect("backend construction");

    wait_ready(&clip, READY_TIMEOUT);
    let first = wait_read(&clip, "one", READ_TIMEOUT);
    let revision = clip.wait_for_change(0, Duration::from_millis(200));

    clip.write("local-before-disconnect", first.generation)
        .expect("write against the first connection");
    assert!(wait_for(Duration::from_secs(2), || {
        factory.lock().unwrap().snapshot().selections == 1
    }));

    // Compositor disappears: the cached text must not survive the session.
    factory.lock().unwrap().shutdown_latest();
    assert!(
        wait_for(Duration::from_secs(3), || clip.info().status
            != ProviderStatus::Ready),
        "status stayed Ready after the compositor disappeared"
    );
    assert!(clip.read().is_err(), "cached text survived the disconnect");
    let after_disconnect = clip.wait_for_change(revision, Duration::from_secs(2));
    assert!(
        after_disconnect > revision,
        "status change did not bump the change revision: {revision} -> {after_disconnect}"
    );

    // Reconnect delivers the new compositor's selection.
    assert!(
        wait_for(Duration::from_secs(10), || clip.info().status
            == ProviderStatus::Ready),
        "backend never reconnected: {:?}",
        clip.info()
    );
    let second = wait_read(&clip, "two", READ_TIMEOUT);
    assert_ne!(
        second.session, first.session,
        "session must change across compositor connections"
    );

    // The new session must not replay the clipboard write from the old one.
    assert!(
        wait_for(Duration::from_secs(1), || factory
            .lock()
            .unwrap()
            .servers
            .len()
            >= 2),
        "connector was not invoked for the new compositor"
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        factory.lock().unwrap().snapshot().selections,
        0,
        "the previous write was replayed on the new connection"
    );
}

#[test]
fn stalled_paste_is_bounded_and_does_not_block_source_reuse() {
    let app = launch(
        None,
        ServerSpec::new(true, false, Some(text_selection("seed"))),
    );
    let seed = wait_read(&app.clip, "seed", READ_TIMEOUT);
    let text = "x".repeat(65536);
    app.clip.write(&text, seed.generation).unwrap();
    let (reply, result) = mpsc::channel();
    app.send(Command::HoldPaste(reply));
    let mut consumer = result.recv_timeout(Duration::from_secs(1)).unwrap();
    // Give the worker a full pipe and do not read it. A stale command must still
    // receive a prompt response while that consumer applies backpressure.
    let started = Instant::now();
    assert!(app.clip.write("must-not-apply", u64::MAX).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        wait_for(super::IO_TIMEOUT + Duration::from_secs(1), || {
            let mut poll = libc::pollfd {
                fd: consumer.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut poll, 1, 0) };
            poll.revents & libc::POLLHUP != 0
        }),
        "stalled source transfer never closed its descriptor"
    );
    let mut partial = Vec::new();
    consumer.read_to_end(&mut partial).unwrap();
    assert!(!partial.is_empty() && partial.len() < text.len());
    assert_eq!(partial, text.as_bytes()[..partial.len()]);
    // Closing one slow consumer must not destroy the reusable selection source.
    assert_eq!(app.fetch("text/plain").unwrap(), text.as_bytes());
}

#[test]
fn seat_removal_and_device_finished_invalidate_and_reconnect() {
    for command in [Command::RemoveSeat, Command::FinishDevice] {
        let app = launch(
            None,
            ServerSpec::new(true, false, Some(text_selection("old-session"))),
        );
        app.factory.lock().unwrap().queue(ServerSpec::new(
            true,
            false,
            Some(text_selection("new-session")),
        ));
        let first = wait_read(&app.clip, "old-session", READ_TIMEOUT);
        app.send(command);
        assert!(wait_for(Duration::from_secs(1), || app.clip.info().status
            != ProviderStatus::Ready));
        assert!(
            app.clip.read().is_err(),
            "old selection survived device/seat loss"
        );
        let next = wait_read(&app.clip, "new-session", Duration::from_secs(5));
        assert_ne!(next.session, first.session);
        assert_eq!(app.snapshot().selections, 0, "reconnect replayed a write");
    }
}
