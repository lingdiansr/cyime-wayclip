# CyIME Wayclip

Wayland clipboard synchronization between a Linux desktop and CyIME on Android.
It supports secure v2 HTTPS/QR pairing, bidirectional text clipboard sync, persistent identity, and a systemd user service.

> **Status:** 0.1.0 preview release. Text clipboard synchronization is supported; binary clipboard data, active push notifications, and complete multi-device mesh semantics are not yet implemented.

## Features

- Wayland clipboard read/write (no X11 fallback)
- Android v2 QR pairing over HTTPS with certificate fingerprint pinning
- Bearer-authenticated status and clipboard APIs
- ETag-based polling and stale-update protection
- Persistent config, TLS identity, and server identity
- systemd user-service integration
- Linux x86_64 release tarball plus Debian/RPM packaging metadata

## Requirements

- Linux with a Wayland compositor and a working clipboard data-control protocol
- Rust stable (for source builds)
- Android CyIME build containing the native v2 clipboard integration
- Same trusted LAN/Wi-Fi for QR pairing

## Install

Download the Linux x86_64 archive from the [0.1.0 release](https://github.com/lingdiansr/cyime-wayclip/releases/tag/v0.1.0), or build from source:

```sh
cargo build --release --all-features
install -Dm755 target/release/cyime-wayclip ~/.local/bin/cyime-wayclip
```

For Debian/RPM builders, use `packaging/debian/` or `packaging/rpm.spec`.

## Configure and run

Initialize the persistent configuration:

```sh
cyime-wayclip config init
cyime-wayclip doctor
```

Run as a user service:

```sh
cyime-wayclip service install
cyime-wayclip service status
```

The service runs in the logged-in graphical session. It is not a system service and cannot access a user clipboard while logged out. To remove it:

```sh
cyime-wayclip service uninstall
```

## Pair Android and sync

Start the v2 pairing server on the LAN interface:

```sh
cyime-wayclip serve-v2
```

Scan the printed QR code in CyIME's clipboard-sync settings. Keep the terminal running while pairing. After pairing, test both directions:

1. Copy text on Linux and wait for it to appear on Android.
2. Copy text on Android and verify it appears in the Wayland clipboard.

The default v2 listener is `0.0.0.0:9876`; pass an explicit address such as `10.42.0.1:9876` when needed. Only expose it on a trusted private network. The v1 HTTP server is legacy and insecure on hostile networks.

## Persistent data

- Config: `${XDG_CONFIG_HOME:-~/.config}/cyime-wayclip/config.toml`
- TLS identity and server state: `${XDG_STATE_HOME:-~/.local/state}/cyime-wayclip/`
- TLS private key is stored with mode `0600` on Unix.

Back up these files if you need to preserve the server identity across reinstalls. Treat them as sensitive.

## Troubleshooting

```sh
cyime-wayclip doctor
cyime-wayclip service status
journalctl --user -u cyime-wayclip.service -e
```

If pairing fails, regenerate the QR code; pairing offers are single-use and expire. Ensure the phone and PC are on the same Wi-Fi, the PC address printed in the QR is reachable, and TCP port 9876 is allowed by the firewall.

## Development

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

CI runs these checks on every push and pull request. Pushing a `v*` tag builds a Linux x86_64 release artifact.

## License

MIT
