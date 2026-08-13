# mcplex 0.5.0 (public beta)

mcplex is a single-user, local MCP gateway. Each configured stdio or Streamable HTTP
server gets its own loopback Streamable HTTP endpoint at `/mcp/{server-id}`. Names and
URIs from the upstream server are preserved unchanged. Linux and macOS are supported.

## Quickstart

```sh
mcplex import ~/.config/Claude/claude_desktop_config.json
mcplex serve --foreground
mcplex snippet claude-code --server github
```

Paste the generated JSON into Claude Code, Cursor, or Claude Desktop. Imports currently
read Claude-style stdio entries; HTTP servers can be added with `mcplex add`.

## Commands

- `serve`, `status`, `doctor`, `reload`, and `tui` operate the daemon. Release
  archives also include `mcplex-daemon` as the dedicated service executable.
- `ls` lists configured servers; `ls --tools --server ID` queries one dedicated
  endpoint; `logs [-f] [--server ID]` reads logs.
- `import [PATH]` imports stdio entries; `snippet CLIENT --server ID` prints client
  configuration for that server's dedicated endpoint.
- `add ID --command CMD [--arg ARG] [--env KEY=VALUE]` adds stdio; `add ID --url URL
  [--header KEY=VALUE] [--oauth] [--scope SCOPE]` adds HTTP. Both accept repeatable
  `--tag` and `--disabled`. Exactly one transport is required; IDs are not overwritten.
- `auth login ID` performs an OAuth 2.1 Authorization Code + PKCE browser flow;
  `auth logout ID` removes that upstream's stored credentials.
- `rm ID`, `enable ID`, and `disable ID` edit/control servers.
- `secret set SERVICE/ACCOUNT [--stdin]` stores a non-empty OS-keyring value without
  echo; `secret rm SERVICE/ACCOUNT` removes it. `keychain:` is accepted on references.

Use global `--config PATH` with every command. Run any command with `--help` for details.

## Configuration and secrets

The default is the platform config directory's `mcplex/config.toml`; a missing file means
an empty valid config.

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
oauth = { scopes = [] }
enabled = true
```

Environment/header values can be literal, `env:NAME`, or `keychain:service/account`.
Literal credentials are discouraged. The authenticated control API token comes from
`MCPLEX_CONTROL_TOKEN` when non-empty. Otherwise macOS uses a generated `config.token`
sibling with owner-only permissions, while other platforms use the OS keyring. MCP
endpoints are unauthenticated.

Config writes are atomic and private on Unix. A sibling advisory lock serializes CLI and
daemon updates. Config changes are hot-reloaded; changing bind/port requires a restart.

OAuth HTTP upstreams use rmcp's OAuth 2.1 implementation: protected-resource and
authorization-server discovery, Dynamic Client Registration, PKCE S256, RFC 8707
resource binding, issuer validation, automatic refresh, and OS-keyring persistence. The
signed macOS CLI and daemon share a narrowly scoped designated requirement so credentials
created by either are available to both without a first-access prompt. Credentials from
versions before 0.5.2 are deliberately not accessed; run `auth login` again after upgrading.
Explicit `keychain:` references remain subject to macOS authorization when another code
identity created them. The browser callback binds a random loopback port for at most five
minutes. Linear example:

```sh
mcplex add linear --url https://mcp.linear.app/mcp --oauth
mcplex auth login linear
mcplex serve --foreground
mcplex snippet claude-code --server linear
# Or configure directly:
claude mcp add --scope user --transport http linear http://127.0.0.1:45850/mcp/linear
```

## Dedicated transparent endpoints

Only `/mcp/{server-id}` is served; the former aggregate `/mcp` endpoint is removed.
Legacy downstream sessions connect to independent upstream sessions. MCP `2026-07-28`
requests use the revision's stateless discover lifecycle and self-contained request
metadata. Both paths preserve names and URIs without public prefixes or collision
rewriting. Unknown and removed IDs return HTTP 404. See the
[protocol support matrix](docs/protocol-support.md) for modern MRTR, tasks, subscriptions,
and transport details.

```sh
mcplex snippet claude-code --server github
mcplex snippet cursor --server linear
```

## TUI and scope

`mcplex tui` shows server state, counts, latency, and bounded logs. Keys: `j/k` or arrows
select, `e` enables/disables, `r` restarts, `R` reloads, `f` filters logs, `?` shows help,
and `q`/Escape quits.

Explicit v0 non-goals are multi-user service, non-loopback binding, and TLS termination.
See [protocol support](docs/protocol-support.md), [security](SECURITY.md), and optional
[user-service samples](docs/services.md).

## Development and release

CI runs formatting, clippy, and tests on Linux and macOS. Releases and Homebrew updates
are automated as described in [docs/releasing.md](docs/releasing.md). Windows is not
currently supported. Licensed MIT OR Apache-2.0.
