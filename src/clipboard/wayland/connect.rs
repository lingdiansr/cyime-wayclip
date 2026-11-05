use super::{Connection, HANDSHAKE_TIMEOUT};
use std::{
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::PathBuf,
    time::Instant,
};

pub(super) fn environment() -> Result<Connection, String> {
    // The library consumes the inherited descriptor and removes WAYLAND_SOCKET,
    // as prescribed by libwayland. No connect syscall is needed on this path.
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        return Connection::connect_to_env().map_err(|e| e.to_string());
    }
    let name = std::env::var_os("WAYLAND_DISPLAY")
        .map(PathBuf::from)
        .ok_or("WAYLAND_DISPLAY is not set")?;
    let path = if name.is_absolute() {
        name
    } else {
        let directory = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or("XDG_RUNTIME_DIR is not an absolute path")?;
        directory.join(name)
    };
    connect(&path)
        .and_then(|stream| {
            Connection::from_socket(stream).map_err(|e| io::Error::other(e.to_string()))
        })
        .map_err(|e| e.to_string())
}

fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address = unsafe { mem::zeroed::<libc::sockaddr_un>() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Wayland socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            (mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        // EAGAIN on AF_UNIX means a full accept backlog, not an in-progress
        // connection. Return to the worker's bounded reconnect backoff instead.
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Wayland connect timed out",
                ));
            }
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            let mut error: libc::c_int = 0;
            let mut length = mem::size_of_val(&error) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut libc::c_int).cast(),
                    &mut length,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            break;
        }
    }
    Ok(UnixStream::from(fd))
}
