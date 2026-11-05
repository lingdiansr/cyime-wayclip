use super::*;
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    os::fd::{AsFd, FromRawFd, OwnedFd},
};
use wayland_client::{
    backend::ObjectId,
    protocol::{wl_callback::WlCallback, wl_registry::WlRegistry, wl_seat::WlSeat},
    EventQueue, Proxy, QueueHandle,
};

mod protocols;
use protocols::{Device, Manager, Offer, Source};

#[derive(Clone, Copy)]
enum SyncPoint {
    Registry,
    Seats,
    Device,
    Write(u64),
}

struct Seat {
    proxy: WlSeat,
    name: Option<String>,
}
struct Offered {
    proxy: Offer,
    mimes: Vec<String>,
}
struct Incoming {
    fd: File,
    generation: u64,
    bytes: Vec<u8>,
    deadline: Instant,
}
struct Outgoing {
    fd: File,
    text: Arc<str>,
    written: usize,
    deadline: Instant,
}
#[derive(Clone)]
struct SourceData {
    id: u64,
    text: Arc<str>,
}
struct PendingWrite {
    id: u64,
    deadline: Instant,
    reply: SyncSender<Result<(), String>>,
}

struct State {
    shared: Arc<Shared>,
    registry: WlRegistry,
    wanted_seat: Option<String>,
    seats: BTreeMap<u32, Seat>,
    ext: Option<(u32, u32)>,
    wlr: Option<(u32, u32)>,
    manager: Option<Manager>,
    manager_global: Option<u32>,
    device: Option<Device>,
    selected_seat: Option<u32>,
    offers: HashMap<ObjectId, Offered>,
    sources: HashMap<u64, Source>,
    incoming: Option<Incoming>,
    outgoing: Vec<Outgoing>,
    write: Option<PendingWrite>,
    next_source: u64,
    registry_done: bool,
    seats_sync: bool,
    initialized: bool,
    selection_seen: bool,
    handshake_deadline: Instant,
    failure: Option<String>,
}

impl State {
    fn configure(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.device.is_some() {
            return;
        }
        let names = self
            .seats
            .iter()
            .map(|(id, seat)| seat.name.clone().unwrap_or_else(|| format!("seat@{id}")))
            .collect::<Vec<_>>();
        let selected = if let Some(wanted) = &self.wanted_seat {
            self.seats.iter().find_map(|(id, seat)| {
                (seat.name.as_deref() == Some(wanted.as_str())).then_some(*id)
            })
        } else if self.seats.len() == 1 {
            self.seats.keys().next().copied()
        } else {
            None
        };
        let reason = if self.ext.is_none() && self.wlr.is_none() {
            Some("compositor advertises no ext/wlr data-control protocol".to_string())
        } else if selected.is_none() {
            Some(if let Some(wanted) = &self.wanted_seat {
                format!(
                    "requested Wayland seat {wanted:?} is unavailable; seats: {}",
                    names.join(", ")
                )
            } else if names.is_empty() {
                "compositor advertises no Wayland seat".into()
            } else {
                format!(
                    "multiple Wayland seats; set CYIME_WAYCLIP_SEAT explicitly: {}",
                    names.join(", ")
                )
            })
        } else {
            None
        };
        if let Some(reason) = reason {
            self.initialized = true;
            self.shared.update(|state| {
                state.info.status = ProviderStatus::Unsupported;
                state.info.message = reason;
                state.info.seats = names;
                state.pending = false;
            });
            return;
        }
        let selected = selected.unwrap();
        let seat = &self.seats[&selected].proxy;
        let (global, manager) = if let Some((id, version)) = self.ext {
            (id, Manager::ext(&self.registry, id, version, qh))
        } else {
            let (id, version) = self.wlr.unwrap();
            (id, Manager::wlr(&self.registry, id, version, qh))
        };
        let device = manager.device(seat, qh);
        let protocol = manager.protocol();
        let seat_name = self.seats[&selected]
            .name
            .clone()
            .unwrap_or_else(|| format!("seat@{selected}"));
        self.shared.update(|state| {
            state.info.status = ProviderStatus::Starting;
            state.info.protocol = Some(protocol);
            state.info.seat = Some(seat_name);
            state.info.seats = names;
            state.info.message = "binding native Wayland data-control device".into();
            state.pending = true;
        });
        self.manager_global = Some(global);
        self.selected_seat = Some(selected);
        self.manager = Some(manager);
        self.device = Some(device);
        self.initialized = false;
        self.handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        conn.display().sync(qh, SyncPoint::Device);
    }

    fn request_configuration(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.registry_done && self.device.is_none() && !self.seats_sync {
            self.seats_sync = true;
            conn.display().sync(qh, SyncPoint::Seats);
        }
    }

    fn offer(&mut self, offer: Offer) {
        if self.offers.len() >= MAX_TRANSFERS * 2 {
            offer.destroy();
            self.failure = Some("too many pending Wayland offers".into());
            return;
        }
        self.offers.insert(
            offer.id(),
            Offered {
                proxy: offer,
                mimes: Vec::new(),
            },
        );
    }

    fn mime(&mut self, id: ObjectId, mime: String) {
        if mime.len() <= 256 && TEXT_MIMES.contains(&mime.as_str()) {
            if let Some(offer) = self.offers.get_mut(&id) {
                if !offer.mimes.contains(&mime) {
                    offer.mimes.push(mime);
                }
            }
        }
    }

    fn ignore_offer(&mut self, offer: Option<Offer>) {
        if let Some(offer) = offer {
            if let Some(entry) = self.offers.remove(&offer.id()) {
                entry.proxy.destroy();
            } else {
                offer.destroy();
            }
        }
    }

    fn selection(&mut self, offer: Option<Offer>) {
        self.selection_seen = true;
        self.incoming = None; // close the previous FD before accepting a new offer
        let generation = self.shared.selection(offer.is_some());
        let Some(offer) = offer else {
            return;
        };
        let entry = self.offers.remove(&offer.id());
        let mime = entry.as_ref().and_then(|entry| {
            TEXT_MIMES
                .iter()
                .find(|mime| entry.mimes.iter().any(|m| m == **mime))
                .copied()
        });
        let Some(mime) = mime else {
            offer.destroy();
            self.shared.finish_read(generation, Ok(None));
            return;
        };
        let result = read_pipe().map(|(read, write)| {
            offer.receive(mime, write.as_fd());
            self.incoming = Some(Incoming {
                fd: File::from(read),
                generation,
                bytes: Vec::with_capacity(4096),
                deadline: Instant::now() + IO_TIMEOUT,
            });
        });
        offer.destroy();
        if let Err(error) = result {
            self.shared.finish_read(generation, Err(error.to_string()));
        }
    }

    fn send(&mut self, fd: OwnedFd, mime: &str, text: &Arc<str>) {
        // Reserve one transfer slot for a new incoming selection, including self-paste.
        if !TEXT_MIMES.contains(&mime) || self.outgoing.len() >= MAX_TRANSFERS - 1 {
            return;
        }
        if nonblocking(fd.as_raw_fd()).is_err() {
            return;
        }
        self.outgoing.push(Outgoing {
            fd: File::from(fd),
            text: Arc::clone(text),
            written: 0,
            deadline: Instant::now() + IO_TIMEOUT,
        });
    }

    fn cancelled(&mut self, id: u64) {
        if let Some(source) = self.sources.remove(&id) {
            source.destroy();
        }
        if self.write.as_ref().is_some_and(|write| write.id == id) {
            self.finish_write(Err(
                "Wayland selection was replaced before confirmation".into()
            ));
        }
    }

    fn finish_write(&mut self, result: Result<(), String>) {
        if let Some(write) = self.write.take() {
            let _ = write.reply.try_send(result);
        }
    }

    fn commands(
        &mut self,
        receiver: &Receiver<WriteCommand>,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // A producer may refill the bounded channel while we drain it. Limit
        // each batch as well, so socket events and FD deadlines cannot starve.
        for _ in 0..COMMAND_CAPACITY {
            let Ok(command) = receiver.try_recv() else {
                break;
            };
            let valid = {
                let state = self.shared.state.lock().unwrap();
                state.info.status == ProviderStatus::Ready
                    && !state.pending
                    && state.error.is_none()
                    && state.generation == command.generation
            };
            let error = if command.deadline <= Instant::now() {
                Some("Wayland write deadline expired")
            } else if !valid {
                Some("Wayland selection changed or is unavailable")
            } else if self.write.is_some() || self.sources.len() >= MAX_TRANSFERS {
                Some("Wayland source is busy")
            } else {
                None
            };
            if let Some(error) = error {
                let _ = command.reply.try_send(Err(error.into()));
                continue;
            }
            let (Some(manager), Some(device)) = (&self.manager, &self.device) else {
                let _ = command
                    .reply
                    .try_send(Err("Wayland data device unavailable".into()));
                continue;
            };
            self.next_source = self.next_source.wrapping_add(1);
            let id = self.next_source;
            let source = manager.source(
                qh,
                SourceData {
                    id,
                    text: command.text,
                },
            );
            for mime in TEXT_MIMES {
                source.offer(mime);
            }
            device.select(&source);
            self.sources.insert(id, source);
            self.write = Some(PendingWrite {
                id,
                deadline: command.deadline,
                reply: command.reply,
            });
            conn.display().sync(qh, SyncPoint::Write(id));
        }
    }

    fn transfers(&mut self) {
        let now = Instant::now();
        if self
            .write
            .as_ref()
            .is_some_and(|write| write.deadline <= now)
        {
            self.finish_write(Err("Wayland write confirmation timed out".into()));
        }
        if let Some(mut incoming) = self.incoming.take() {
            let mut result = None;
            if incoming.deadline <= now {
                result = Some(Err("Wayland selection transfer timed out".into()));
            } else {
                let mut buffer = [0; 4096];
                loop {
                    let remaining = (crate::protocol::MAX_TEXT_BYTES + 1) - incoming.bytes.len();
                    let capacity = buffer.len().min(remaining);
                    match incoming.fd.read(&mut buffer[..capacity]) {
                        Ok(0) => {
                            result = Some(
                                String::from_utf8(std::mem::take(&mut incoming.bytes))
                                    .map(|text| Some(Arc::<str>::from(text)))
                                    .map_err(|_| "Wayland selection is not valid UTF-8".into()),
                            );
                            break;
                        }
                        Ok(n) => {
                            incoming.bytes.extend_from_slice(&buffer[..n]);
                            if incoming.bytes.len() > crate::protocol::MAX_TEXT_BYTES {
                                result = Some(Err("Wayland selection exceeds 65536 bytes".into()));
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            result = Some(Err(e.to_string()));
                            break;
                        }
                    }
                }
            }
            if let Some(result) = result {
                self.shared.finish_read(incoming.generation, result);
            } else {
                self.incoming = Some(incoming);
            }
        }
        self.outgoing.retain_mut(|transfer| {
            if transfer.deadline <= now {
                return false;
            }
            while transfer.written < transfer.text.len() {
                match transfer
                    .fd
                    .write(&transfer.text.as_bytes()[transfer.written..])
                {
                    Ok(0) => return false,
                    Ok(n) => transfer.written += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                    Err(_) => return false,
                }
            }
            false // EOF is part of the transfer protocol, so close promptly.
        });
    }

    fn timeout(&self) -> i32 {
        let mut deadline = Instant::now() + Duration::from_secs(1);
        if !self.initialized {
            deadline = deadline.min(self.handshake_deadline);
        }
        if let Some(incoming) = &self.incoming {
            deadline = deadline.min(incoming.deadline);
        }
        if let Some(write) = &self.write {
            deadline = deadline.min(write.deadline);
        }
        for transfer in &self.outgoing {
            deadline = deadline.min(transfer.deadline);
        }
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1) as i32
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.finish_write(Err("Wayland connection stopped".into()));
        // OwnedFd and proxy ownership are confined to this worker. Dropping the
        // connection below also releases every compositor-side object.
    }
}

pub(super) fn run(
    connection: Connection,
    shared: &Arc<Shared>,
    wake: &mut UnixStream,
    receiver: &Receiver<WriteCommand>,
    wanted_seat: Option<String>,
) -> Result<(), String> {
    let mut queue: EventQueue<State> = connection.new_event_queue();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    connection.display().sync(&qh, SyncPoint::Registry);
    let mut state = State {
        shared: Arc::clone(shared),
        registry,
        wanted_seat,
        seats: BTreeMap::new(),
        ext: None,
        wlr: None,
        manager: None,
        manager_global: None,
        device: None,
        selected_seat: None,
        offers: HashMap::new(),
        sources: HashMap::new(),
        incoming: None,
        outgoing: Vec::with_capacity(MAX_TRANSFERS - 1),
        write: None,
        next_source: 0,
        registry_done: false,
        seats_sync: false,
        initialized: false,
        selection_seen: false,
        handshake_deadline: Instant::now() + HANDSHAKE_TIMEOUT,
        failure: None,
    };
    while !shared.stop.load(Ordering::Acquire) {
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| e.to_string())?;
        if let Some(error) = state.failure.take() {
            return Err(error);
        }
        if !state.initialized && Instant::now() >= state.handshake_deadline {
            return Err("Wayland initialization timed out".into());
        }
        state.commands(receiver, &connection, &qh);
        state.transfers();
        let blocked = match connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let empty = libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        };
        let mut polls = [empty; MAX_TRANSFERS + 2];
        polls[0] = libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN | if blocked { libc::POLLOUT } else { 0 },
            revents: 0,
        };
        polls[1] = libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let mut count = 2;
        if let Some(incoming) = &state.incoming {
            polls[count] = libc::pollfd {
                fd: incoming.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            count += 1;
        }
        for outgoing in &state.outgoing {
            polls[count] = libc::pollfd {
                fd: outgoing.fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            count += 1;
        }
        let ready =
            unsafe { libc::poll(polls.as_mut_ptr(), count as libc::nfds_t, state.timeout()) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            drop(guard);
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if polls[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        } else {
            drop(guard);
        }
        if polls[1].revents & libc::POLLIN != 0 {
            drain_wake(wake);
        }
    }
    Ok(())
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    // Only our read end is nonblocking. External clipboard owners may use a
    // conventional blocking write_all on the descriptor handed to them.
    nonblocking(read.as_raw_fd())?;
    Ok((read, write))
}
