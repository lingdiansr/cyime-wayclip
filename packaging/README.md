# User service

Install the unit to `~/.config/systemd/user/cyime-wayclip.service`, then run:

```sh
systemctl --user daemon-reload
systemctl --user enable --now cyime-wayclip.service
```

The service is intentionally a user service: it requires an active graphical
session and does not promise clipboard access while logged out. It currently
reports `Unsupported` until a compositor-specific Wayland provider is enabled.
