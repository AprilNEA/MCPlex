# Optional user services

Nothing is installed automatically. Edit executable paths and token handling first.
Release archives and `cargo install` include the dedicated `mcplex-daemon` executable;
`mcplex serve --foreground` remains available for interactive use.

Linux: copy `docs/mcplex.service` to `~/.config/systemd/user/`, then run `systemctl --user daemon-reload &&
systemctl --user enable --now mcplex`. Inspect with `journalctl --user -u mcplex`.
Secret Service must be running and its collection unlocked when keyring mode is used.
For a headless service, alternatively add `EnvironmentFile=%h/.config/mcplex/environment`
under `[Service]`, put `MCPLEX_CONTROL_TOKEN=...` (and upstream secrets) in that file,
and set its permissions to 0600.

macOS: replace every `/Users/YOU` in `docs/com.mcplex.daemon.plist`, copy it to
`~/Library/LaunchAgents/`, then run `launchctl bootstrap gui/$(id -u)
~/Library/LaunchAgents/com.mcplex.daemon.plist`. Stop with `launchctl bootout
gui/$(id -u)/com.mcplex.daemon`.
