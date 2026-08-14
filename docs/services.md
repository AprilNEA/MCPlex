# Optional user services

`mcplex serve --foreground` remains available for interactive use. Release archives
and `cargo install` also include the dedicated `mcplex-daemon` executable.

Homebrew installations can run it persistently as a user service:

```sh
brew services start AprilNEA/tap/mcplex
brew services info AprilNEA/tap/mcplex
```

The service starts at login. Its output is written to Homebrew's
`var/log/mcplex.log`. Stop it with `brew services stop AprilNEA/tap/mcplex`.

Linux: copy `docs/mcplex.service` to `~/.config/systemd/user/`, then run `systemctl --user daemon-reload &&
systemctl --user enable --now mcplex`. Inspect with `journalctl --user -u mcplex`.
Secret Service must be running and its collection unlocked when keyring mode is used.
For a headless service, alternatively add `EnvironmentFile=%h/.config/mcplex/environment`
under `[Service]`, put `MCPLEX_CONTROL_TOKEN=...` (and upstream secrets) in that file,
and set its permissions to 0600. This manual setup is unnecessary when using the
Homebrew service.

For a non-Homebrew macOS install, replace every `/Users/YOU` in
`docs/com.aprilnea.mcplex.daemon.plist`, copy it to `~/Library/LaunchAgents/`, then run
`launchctl bootstrap gui/$(id -u)
~/Library/LaunchAgents/com.aprilnea.mcplex.daemon.plist`. Stop with
`launchctl bootout gui/$(id -u)/com.aprilnea.mcplex.daemon`.
