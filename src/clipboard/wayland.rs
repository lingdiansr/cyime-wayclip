//! A single native Wayland owner, shared through immutable selection snapshots.
use super::{ClipboardSnapshot, ProviderStatus};
use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use wayland_client::Connection;

mod connect;
mod native;

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TRANSFERS: usize = 16;
const COMMAND_CAPACITY: usize = 16;
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];

#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub status: ProviderStatus,
    pub protocol: Option<&'static str>,
    pub seat: Option<String>,
    pub seats: Vec<String>,
    pub message: String,
}

#[derive(Clone)]
pub struct WaylandClipboard {
    inner: Arc<Handle>,
}

struct Published {
    info: BackendInfo,
    generation: u64,
    session: u64,
    revision: u64,
    text: Option<Arc<str>>,
    pending: bool,
    error: Option<String>,
}

struct Shared {
    state: Mutex<Published>,
    changed: Condvar,
    stop: AtomicBool,
}

impl Shared {
    fn update<T>(&self, change: impl FnOnce(&mut Published) -> T) -> T {
        let mut state = self.state.lock().unwrap();
        let result = change(&mut state);
        state.revision = state.revision.wrapping_add(1);
        drop(state);
        self.changed.notify_all();
        result
    }

    fn unavailable(&self, status: ProviderStatus, message: String) {
        self.update(|state| {
            state.info.status = status;
            state.info.protocol = None;
            state.info.seat = None;
            state.info.seats.clear();
            state.info.message = message;
            state.generation = state.generation.wrapping_add(1);
            state.text = None;
            state.pending = false;
            state.error = None;
        });
    }

    fn begin_session(&self) {
        self.update(|state| {
            state.session = state.session.wrapping_add(1);
            state.generation = state.generation.wrapping_add(1);
            state.text = None;
            state.error = None;
            state.pending = true;
            state.info.status = ProviderStatus::Starting;
            state.info.message = "enumerating Wayland registry and seats".into();
        });
    }

    fn selection(&self, pending: bool) -> u64 {
        self.update(|state| {
            state.generation = state.generation.wrapping_add(1);
            state.text = None;
            state.pending = pending;
            state.error = None;
            state.generation
        })
    }

    fn finish_read(&self, generation: u64, result: Result<Option<Arc<str>>, String>) {
        self.update(|state| {
            if state.generation != generation {
                return;
            }
            state.pending = false;
            match result {
                Ok(text) => {
                    state.text = text;
                    state.error = None;
                }
                Err(error) => {
                    state.text = None;
                    state.error = Some(error);
                }
            }
        });
    }
}

struct WriteCommand {
    text: Arc<str>,
    generation: u64,
    deadline: Instant,
    reply: SyncSender<Result<(), String>>,
}

struct Handle {
    shared: Arc<Shared>,
    commands: SyncSender<WriteCommand>,
    wake: Mutex<UnixStream>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.wake.lock().unwrap().write(&[1]);
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}

impl WaylandClipboard {
    /// Construction succeeds even when the compositor is not available yet.
    /// Connection and registry failures are observable through info()/read().
    pub fn start(seat: Option<String>) -> io::Result<Self> {
        start_with_connector(seat, connect::environment)
    }

    pub fn info(&self) -> BackendInfo {
        let state = self.inner.shared.state.lock().unwrap();
        BackendInfo {
            status: state.info.status,
            protocol: state.info.protocol,
            seat: state.info.seat.clone(),
            seats: state.info.seats.clone(),
            message: state.error.clone().unwrap_or_else(|| {
                if state.pending && state.info.status == ProviderStatus::Ready {
                    "Wayland selection transfer is pending".into()
                } else {
                    state.info.message.clone()
                }
            }),
        }
    }

    /// Never performs Wayland I/O or waits for a clipboard owner.
    pub fn read(&self) -> Result<ClipboardSnapshot, String> {
        let state = self.inner.shared.state.lock().unwrap();
        if state.info.status != ProviderStatus::Ready {
            return Err(state.info.message.clone());
        }
        if state.pending {
            return Err("Wayland selection transfer is pending".into());
        }
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        Ok(ClipboardSnapshot {
            generation: state.generation,
            session: state.session,
            text: state.text.clone(),
        })
    }

    /// Installs a new source only if the previously observed selection is current.
    /// Success means the compositor processed set_selection, not that somebody pasted.
    pub fn write(&self, text: &str, expected_generation: u64) -> Result<(), String> {
        if text.len() > crate::protocol::MAX_TEXT_BYTES
            || text.contains('\0')
            || text.trim().is_empty()
        {
            return Err("invalid clipboard text".into());
        }
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let (reply, result) = mpsc::sync_channel(1);
        self.inner
            .commands
            .try_send(WriteCommand {
                text: Arc::from(text),
                generation: expected_generation,
                deadline,
                reply,
            })
            .map_err(|_| "Wayland command queue is full or unavailable".to_string())?;
        match self.inner.wake.lock().unwrap().write(&[1]) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.to_string()),
        }
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "Wayland write timed out or worker stopped".to_string())?
    }

    /// Revision includes completed reads and status changes, unlike selection generation.
    pub fn wait_for_change(&self, revision: u64, timeout: Duration) -> u64 {
        let state = self.inner.shared.state.lock().unwrap();
        let (state, _) = self
            .inner
            .shared
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.revision == revision && !self.inner.shared.stop.load(Ordering::Acquire)
            })
            .unwrap();
        state.revision
    }
}

fn start_with_connector<F>(seat: Option<String>, mut connector: F) -> io::Result<WaylandClipboard>
where
    F: FnMut() -> Result<Connection, String> + Send + 'static,
{
    let shared = Arc::new(Shared {
        state: Mutex::new(Published {
            info: BackendInfo {
                status: ProviderStatus::Starting,
                protocol: None,
                seat: None,
                seats: Vec::new(),
                message: "starting native Wayland worker".into(),
            },
            generation: 0,
            session: 0,
            revision: 0,
            text: None,
            pending: true,
            error: None,
        }),
        changed: Condvar::new(),
        stop: AtomicBool::new(false),
    });
    let (wake, mut incoming) = UnixStream::pair()?;
    wake.set_nonblocking(true)?;
    incoming.set_nonblocking(true)?;
    let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
    let worker_shared = Arc::clone(&shared);
    let worker = std::thread::Builder::new()
        .name("wayland-clipboard".into())
        .spawn(move || {
            let mut attempt = 0usize;
            while !worker_shared.stop.load(Ordering::Acquire) {
                if attempt > 0 {
                    worker_shared.unavailable(
                        ProviderStatus::Reconnecting,
                        "reconnecting to Wayland compositor".into(),
                    );
                }
                let result = connector().and_then(|connection| {
                    worker_shared.begin_session();
                    native::run(
                        connection,
                        &worker_shared,
                        &mut incoming,
                        &receiver,
                        seat.clone(),
                    )
                });
                if worker_shared.stop.load(Ordering::Acquire) {
                    break;
                }
                if worker_shared.state.lock().unwrap().info.status == ProviderStatus::Ready {
                    attempt = 0;
                }
                worker_shared.unavailable(
                    ProviderStatus::Disconnected,
                    result
                        .err()
                        .unwrap_or_else(|| "Wayland device finished".into()),
                );
                // A connection that reached Ready starts a fresh failure sequence.
                let delays = [1u64, 2, 5, 10, 30];
                let delay = Duration::from_secs(delays[attempt.min(delays.len() - 1)]);
                attempt = (attempt + 1).min(delays.len() - 1);
                let jitter =
                    Duration::from_millis(crate::sync_runtime::now_ms().unsigned_abs() % 101);
                let retry_at = Instant::now() + delay + jitter;
                while !worker_shared.stop.load(Ordering::Acquire) && Instant::now() < retry_at {
                    reject_commands(&receiver, "Wayland compositor unavailable");
                    let remaining = retry_at.saturating_duration_since(Instant::now());
                    let mut poll = libc::pollfd {
                        fd: incoming.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(100) as i32) };
                    drain_wake(&mut incoming);
                }
            }
            reject_commands(&receiver, "Wayland worker stopped");
        })?;
    Ok(WaylandClipboard {
        inner: Arc::new(Handle {
            shared,
            commands,
            wake: Mutex::new(wake),
            worker: Mutex::new(Some(worker)),
        }),
    })
}

fn reject_commands(receiver: &Receiver<WriteCommand>, message: &str) {
    for _ in 0..COMMAND_CAPACITY {
        let Ok(command) = receiver.try_recv() else {
            break;
        };
        let _ = command.reply.try_send(Err(message.into()));
    }
}

fn drain_wake(stream: &mut UnixStream) {
    let mut buffer = [0; 128];
    for _ in 0..COMMAND_CAPACITY {
        let Ok(n) = stream.read(&mut buffer) else {
            break;
        };
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
#[path = "wayland_tests.rs"]
mod tests;
