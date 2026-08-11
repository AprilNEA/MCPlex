# mcplex 0.4.0 (public beta)

mcplex is a single-user, local MCP multiplexer: multiple stdio or Streamable HTTP
upstreams become one loopback Streamable HTTP endpoint. Linux and macOS are supported.

## Quickstart: import, serve, snippet

```sh
mcplex import ~/.config/Claude/claude_desktop_config.json
mcplex serve --foreground                 # or configure docs/services.md
mcplex snippet claude-code
```

Paste the printed JSON into Claude Code, Cursor, or Claude Desktop (select that client
for its shape), then connect it to `http://127.0.0.1:45850/mcp`. Imports currently read
Claude-style stdio entries; HTTP servers can be added with `mcplex add`.

## Commands

- `serve`, `status`, `doctor`, `reload`, and `tui` operate the daemon.
- `ls [--tools]` lists servers or routed tools; `logs [-f] [--server ID]` reads logs.
- `import [PATH]` imports stdio entries; `snippet CLIENT` prints client configuration.
- `add ID --command CMD [--arg ARG] [--env KEY=VALUE]` adds stdio; `add ID --url URL
  [--header KEY=VALUE]` adds HTTP. Both accept `--alias`, repeatable `--tag`, and
  `--disabled`. Exactly one transport is required and existing IDs are never overwritten.
- `rm ID`, `enable ID`, and `disable ID` edit/control servers.
- `secret set SERVICE/ACCOUNT [--stdin]` stores a non-empty OS-keyring value without
  echo; `secret rm SERVICE/ACCOUNT` removes it. `keychain:` is accepted on references.

Use global `--config PATH` with every command. Run any command with `--help` for details.

## Configuration and secrets

The default is the platform config directory's `mcplex/config.toml`; a missing file means
an empty, valid config. Example:

```toml
[daemon]
bind = "127.0.0.1"
port = 45850

[servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@example/mcp-server"]
env = { TOKEN = "env:GITHUB_TOKEN" }
tags = ["work"]

[servers.remote]
transport = "http"
url = "https://example.test/mcp"
headers = { Authorization = "keychain:mcplex/example-bearer" }
alias = "example"
enabled = true
```

Environment/header values can be literal, `env:NAME`, or `keychain:service/account`.
Literal credentials are discouraged. The authenticated control API token comes from
`MCPLEX_CONTROL_TOKEN` when non-empty, otherwise OS keyring entry
`mcplex/control-token` (created if absent). The MCP endpoint itself is unauthenticated.
`rpassword` is used solely for maintained, no-echo terminal secret input.

Atomic config replacements are watched and hot-reloaded. A sibling advisory lock file
(implemented with the maintained `fs2` crate) serializes config read-modify-write updates
across CLI and daemon processes. `add`, `rm`, enable/disable,
`reload`, and SIGHUP apply upstream changes. Changing bind/port requires process restart;
a rejected reload is reported. Offline edits succeed when no daemon is listening.

## Aggregate and dedicated MCP endpoints

The exact `/mcp` endpoint aggregates every enabled, ready server. Each configured server
also has a stable `/mcp/ID` endpoint exposing only that server's namespaced tools,
resources, and prompts. Disabled or temporarily degraded servers remain addressable with
an empty current catalog; removed or unknown IDs return HTTP 404. Both endpoint forms are
unauthenticated and loopback-only.

Use `mcplex snippet claude-code` for the aggregate, or assign dedicated MCPs to agents:

```sh
mcplex snippet claude-code --server github
mcplex snippet cursor --server linear
```

These produce URLs such as `http://127.0.0.1:45850/mcp/github`, while preserving public
names such as `github__search`.

## TUI

`mcplex tui` shows server state, counts, last-call latency, and bounded incremental logs.
Keys: `j/k` or arrows select, `e` enables/disables, `r` restarts, `R` reloads, `f`
filters logs, `?` shows help, and `q`/Escape quits.

## Scope and comparison

Like 1MCP and McpMux, mcplex addresses the practical “one client connection, several MCP
servers” workflow. mcplex specifically chooses a local-only Rust daemon, stable namespaced
tools/prompts, collision-safe resources, file-based configuration, keyring indirections,
an authenticated control plane, and a terminal dashboard. This is a design comparison,
not a claim about those projects' complete or current feature sets.

Explicit v0 non-goals are OAuth, multi-user service, non-loopback binding, TLS termination,
and sampling/roots passthrough. See [SECURITY.md](SECURITY.md) and optional, non-installing
[user-service samples](docs/services.md).

## Development and release

CI runs formatting, clippy, and tests on Linux and macOS. Release PRs, crates.io
publication, cargo-dist artifacts, GitHub Releases, and the Homebrew tap update are
automated as described in [docs/releasing.md](docs/releasing.md). This repository does
not claim Windows support. Licensed MIT OR Apache-2.0.
