# MCP protocol support

MCPlex targets a dedicated transparent gateway model. The old `/mcp` aggregate
endpoint has been removed. `/mcp/{server-id}` isolates legacy, initialized downstream
sessions with one upstream session each. Modern `2026-07-28` requests are stateless and
use a discover-lifecycle upstream shared by the dedicated server endpoint; required
per-request identity and capabilities keep each forwarded request self-contained. A
long-lived `subscriptions/listen` request owns its upstream subscription connection so
its client metadata and cancellation lifetime cannot be confused with another client.

The matrix distinguishes current behavior from the target rather than treating planned
passthrough as implemented.

| Area | Current status | Transparent-gateway target |
| --- | --- | --- |
| Endpoints | Dedicated `/mcp/{server-id}` only | Legacy sessions stay 1:1; modern requests use the endpoint's stateless upstream. |
| Tools | Supported | Preserve upstream names and payloads unchanged. |
| Resources | Supported | Preserve upstream URIs and payloads unchanged. |
| Resource templates | Supported | Forward list requests and results unchanged. |
| Prompts | Supported | Preserve upstream names and payloads unchanged. |
| Authorization | Supported for HTTP upstreams | Continue OAuth 2.1 discovery, DCR, PKCE, resource binding, issuer validation, keyring persistence, refresh, and static-header alternatives. |
| Ping | Terminated locally | Transparent forwarding is not currently promised. |
| Cancellation and progress | Supported | Request IDs and progress tokens are translated; closing a modern request's SSE response cancels its upstream request. |
| Resource subscriptions/updates | Supported for negotiated versions | Legacy subscribe/unsubscribe and modern `subscriptions/listen` are bridged. |
| Completion | Supported | Forward `completion/complete`. |
| MCP logging | Supported | Forward level requests and log notifications. |
| Roots | Supported | Legacy reverse requests are forwarded; modern requests use MRTR `inputRequests`. |
| Sampling | Supported | Legacy reverse requests are forwarded; modern requests use MRTR `inputRequests`. |
| Elicitation | Supported | Legacy reverse requests are forwarded; modern requests use MRTR `inputRequests`. |
| Tasks | Supported | Forward the `io.modelcontextprotocol/tasks` extension: task results, get, update, and cancel. Polling is supported; task notifications remain limited by rmcp's subscription filter. |
| Custom methods | Supported for requests; legacy notifications supported | Forward custom requests and results in both directions. Stateless downstream notifications are not promised by the current rmcp transport. |
| Discovery | Supported | `server/discover` advertises versions through `2026-07-28` and upstream capabilities. |
| Transports | Partial | Current: upstream stdio and Streamable HTTP; downstream Streamable HTTP. Legacy SSE and WebSocket are not implemented. |

Current support uses rmcp 3.1.2 and advertises revisions through `2026-07-28`.
That revision replaces server-initiated JSON-RPC requests with stateless MRTR
`InputRequiredResult` exchanges, so MCPlex forwards MRTR results and retries instead of
trying to transplant an HTTP request-stream association between terminated connections.
Modern downstream requests require an upstream that supports the discover lifecycle and
`2026-07-28`; MCPlex does not silently reinterpret requests carrying complete modern
per-request metadata as legacy initialized sessions. Conversely, an `initialize` opener
selects the legacy lifecycle even when it offers `2026-07-28`, so MCPlex negotiates that
offer down to its preferred supported legacy revision (`2025-11-25`).
MCPlex uses the publishable `mcplex-rmcp` compatibility crate, vendored from rmcp
3.1.2. The gateway keeps modern request-scoped SSE responses open immediately when JSON
responses are disabled, allowing stream closure to cancel work even before the first
progress or result message. Its focused transport patch rejects malformed modern request
metadata before that stream opens and preserves `Mcp-Param-*` headers across the Gateway
boundary. Ping is intentionally terminated at each
local connection because it is a connection-liveness operation, not an application
request. Legacy SSE and WebSocket are not part of the current Streamable HTTP product
surface.
