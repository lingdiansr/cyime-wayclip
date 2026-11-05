//! Inline QR images with a plain-text fallback for pipes and unsupported terminals.
use base64::{engine::general_purpose::STANDARD, Engine};
use qrcodegen::{QrCode, QrCodeEcc};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

const BORDER: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Kitty,
    Iterm,
    Sixel,
}

pub(crate) fn print(text: &str) {
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return;
    };
    let stdout = io::stdout();
    let protocol = stdout.is_terminal().then(detect_protocol).flatten();
    let mut out = stdout.lock();
    let result = match protocol {
        Some(protocol) => print_image(&mut out, &qr, protocol),
        None => print_blocks(&mut out, &qr),
    };
    if let Err(e) = result {
        eprintln!("QR output: {e}");
    }
}

fn detect_protocol() -> Option<Protocol> {
    // Multiplexers may suppress graphics even when the outer terminal supports it.
    if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
        return None;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM")
        .or_else(|_| std::env::var("LC_TERMINAL"))
        .unwrap_or_default();
    if matches!(program.as_str(), "ghostty" | "kitty" | "WezTerm")
        || matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty")
    {
        return Some(Protocol::Kitty);
    }
    if matches!(
        program.as_str(),
        "iTerm.app" | "iTerm2" | "mintty" | "rio" | "WarpTerminal"
    ) || std::env::var_os("KONSOLE_VERSION").is_some()
    {
        return Some(Protocol::Iterm);
    }
    if term.contains("sixel") {
        return Some(Protocol::Sixel);
    }
    if term.is_empty() || term == "dumb" || term == "linux" {
        return None;
    }
    probe_protocol().ok().flatten()
}

struct TtyMode {
    tty: File,
    original: libc::termios,
}

impl Drop for TtyMode {
    fn drop(&mut self) {
        // Restore canonical input and echo on every return, including I/O errors.
        unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.original) };
    }
}

fn probe_protocol() -> io::Result<Option<Protocol>> {
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    if unsafe { libc::tcgetpgrp(fd) } != unsafe { libc::getpgrp() } {
        return Ok(None);
    }
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut mode = original;
    mode.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
    mode.c_cc[libc::VMIN] = 0;
    mode.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut guard = TtyMode { tty, original };
    // DA1 terminates the probe on ordinary terminals; the deadline also covers
    // terminals that ignore both requests. Never leave startup waiting for input.
    guard
        .tty
        .write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c")?;
    guard.tty.flush()?;
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut response = [0; 512];
    let mut used = 0;
    while used < response.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
        if ready <= 0 || poll.revents & libc::POLLIN == 0 {
            break;
        }
        let n = guard.tty.read(&mut response[used..])?;
        if n == 0 {
            break;
        }
        used += n;
        if response[..used].contains(&b'c') {
            break;
        }
    }
    Ok(parse_response(&response[..used]))
}

fn parse_response(response: &[u8]) -> Option<Protocol> {
    let response = std::str::from_utf8(response).ok()?;
    if response.contains("\x1b_Gi=31;OK\x1b\\") {
        return Some(Protocol::Kitty);
    }
    let (_, attributes) = response.split_once("\x1b[?")?;
    let (attributes, _) = attributes.split_once('c')?;
    attributes
        .split(';')
        .skip(1)
        .any(|p| p == "4")
        .then_some(Protocol::Sixel)
}

fn image_pixels(qr: &QrCode) -> (u32, Vec<u8>) {
    let modules = qr.size() + 2 * BORDER;
    // Integer scaling keeps module edges sharp. Fit a compact square, leaving
    // room for the URI and the next prompt even in a short terminal window.
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    let have_size = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    let mut limit = 400;
    if have_size {
        let width = if size.ws_xpixel > 0 {
            size.ws_xpixel as i32
        } else {
            size.ws_col as i32 * 8
        };
        let height = if size.ws_ypixel > 0 {
            size.ws_ypixel as i32
        } else {
            size.ws_row as i32 * 16
        };
        if width > 0 && height > 64 {
            limit = limit.min(width).min(height - 64);
        }
    }
    let scale = (limit / modules).clamp(1, 6);
    let side = modules * scale;
    let mut pixels = vec![255; (side * side) as usize];
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                let left = (x + BORDER) * scale;
                let top = (y + BORDER) * scale;
                for dy in 0..scale {
                    let start = ((top + dy) * side + left) as usize;
                    pixels[start..start + scale as usize].fill(0);
                }
            }
        }
    }
    (side as u32, pixels)
}

fn print_image(out: &mut impl Write, qr: &QrCode, protocol: Protocol) -> io::Result<()> {
    let (side, pixels) = image_pixels(qr);
    if protocol == Protocol::Sixel {
        return print_sixel(out, side, &pixels);
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, side, side);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()?;
    }
    let encoded = STANDARD.encode(&png);
    match protocol {
        Protocol::Kitty => {
            let mut chunks = encoded.as_bytes().chunks(4096).peekable();
            let mut first = true;
            while let Some(chunk) = chunks.next() {
                write!(out, "\x1b_G")?;
                if first {
                    write!(out, "a=T,f=100,t=d,q=2,")?;
                    first = false;
                }
                write!(out, "m={};", u8::from(chunks.peek().is_some()))?;
                out.write_all(chunk)?;
                out.write_all(b"\x1b\\")?;
            }
            writeln!(out)?;
        }
        Protocol::Iterm => {
            writeln!(out, "\x1b]1337;File=inline=1;size={};width={}px;height={}px;preserveAspectRatio=1:{}\x07", png.len(), side, side, encoded)?;
        }
        Protocol::Sixel => unreachable!(),
    }
    out.flush()
}

fn print_sixel(out: &mut impl Write, side: u32, pixels: &[u8]) -> io::Result<()> {
    write!(
        out,
        "\x1bP0;1;0q\"1;1;{side};{side}#0;2;100;100;100#1;2;0;0;0"
    )?;
    let side = side as usize;
    let mut band = Vec::with_capacity(side);
    for y in (0..side).step_by(6) {
        // Paint white explicitly: neither terminal theme nor transparent Sixel
        // backgrounds may change the quiet zone or the light modules.
        let rows = 6.min(side - y);
        write!(
            out,
            "#0!{}{}$#1",
            side,
            char::from(63 + ((1u8 << rows) - 1))
        )?;
        band.clear();
        for x in 0..side {
            let mut bits = 0;
            for dy in 0..rows {
                if pixels[(y + dy) * side + x] == 0 {
                    bits |= 1 << dy;
                }
            }
            band.push(63 + bits);
        }
        out.write_all(&band)?;
        if y + 6 < side {
            out.write_all(b"-")?;
        }
    }
    out.write_all(b"\x1b\\\n")?;
    out.flush()
}

fn print_blocks(out: &mut impl Write, qr: &QrCode) -> io::Result<()> {
    let mut row = String::with_capacity((qr.size() + 2 * BORDER) as usize * 3);
    for y in (-BORDER..qr.size() + BORDER).step_by(2) {
        row.clear();
        for x in -BORDER..qr.size() + BORDER {
            row.push(match (qr.get_module(x, y), qr.get_module(x, y + 1)) {
                (false, false) => ' ',
                (true, false) => '▀',
                (false, true) => '▄',
                (true, true) => '█',
            });
        }
        writeln!(out, "{row}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_responses_require_complete_positive_reports() {
        assert_eq!(
            parse_response(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4c"),
            Some(Protocol::Kitty)
        );
        assert_eq!(parse_response(b"\x1b[?62;1;4c"), Some(Protocol::Sixel));
        for response in [
            b"\x1b[?62;14c".as_slice(),
            b"\x1b[?4;1c",
            b"\x1b[?62;4",
            b"\x1b_Gi=31;ERROR\x1b\\",
            b"",
        ] {
            assert_eq!(parse_response(response), None, "{response:?}");
        }
    }
}
