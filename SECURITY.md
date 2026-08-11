# Security

mcplex v0 trusts the local user and MCP clients on the same machine. It accepts only a
loopback bind and provides no TLS or multi-user isolation. The MCP and health endpoints
are unauthenticated; the control API uses a bearer token from `MCPLEX_CONTROL_TOKEN` or
the OS keyring. Do not expose the port through a proxy or tunnel.

Use `env:NAME` or `keychain:service/account` for environment and header values. Plain
values in TOML are supported but are plaintext secrets; config writes use atomic private
files on Unix. Secret prompts do not echo and values are redacted from mcplex diagnostics,
but upstream child processes and external software remain within your local trust boundary.

To report a vulnerability, use the repository host's private security-reporting feature
if available. No dedicated reporting address is currently published; avoid public issues
for undisclosed vulnerabilities.
