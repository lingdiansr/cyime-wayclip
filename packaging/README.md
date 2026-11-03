# User service

`cyime-wayclip` runs in the logged-in graphical user session. It is **not** a
system service and does not provide clipboard access while logged out.

Install the unit and initialize configuration:

```sh
install -Dm644 packaging/cyime-wayclip.service \
  "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cyime-wayclip.service"
cyime-wayclip config init
systemctl --user daemon-reload
systemctl --user enable --now cyime-wayclip.service
systemctl --user status cyime-wayclip.service
```

The unit runs the normal `run` command. Use `cyime-wayclip doctor` to inspect
Wayland capability and the effective config path. The v1/v2 `serve` commands
are interactive pairing servers and should be started manually, not from this
unit.
