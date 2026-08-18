# XanhTab protocol v1

`XanhTab` is the source of truth for this protocol. Consumers must negotiate the major version and preserve backward compatibility within v1.

## Trust boundaries

- `xanhtabd` owns HTTP/TLS, zero-account pairing, session state, metrics, and event delivery. It is unprivileged.
- `xanhtab-browser` owns one WPE WebView, its WebKit child processes, and the GStreamer stream tree. It runs as UID 988 in a separate cgroup.
- `xanhtab-netd` is the only privileged component. It accepts the versioned `EgressCommand` enum over a Unix socket and invokes allowlisted executables without a shell.
- Sensitive browsing state belongs under `/run/xanhtab-session`. Non-secret configuration and compiled blocklists may remain on disk.

## Authentication

At boot and after every burn, the daemon creates 256 bits with the OS CSPRNG. The QR URL carries the URL-safe representation in its fragment, so it is not sent in an HTTP request or proxy log. The grouped Base32 manual code represents the same bytes and is accepted as an alternative.

`POST /api/v1/pair/exchange` consumes either representation exactly once. It returns a CSRF token in the response body and sets `xanhtab_session` as `HttpOnly`, `SameSite=Strict`, and `Secure` in production. Mutations require both the cookie and `X-XanhTab-CSRF`. Five failed pairing attempts cause a global 30-second delay.

WebSocket and future WebRTC signaling connections use a one-time, short-lived ticket from `POST /api/v1/webrtc/ticket`. Burn increments the auth generation and invalidates every cookie and outstanding ticket.

## Session state

The state machine is `idle → starting → active → burning → idle`; an observable `failed` state records redacted lifecycle failures. Only the paired client can hold the controller lease. A successful burn:

1. marks the session `burning` and closes event delivery;
2. stops the complete browser/GStreamer cgroup;
3. restores the default egress policy;
4. clears `/run/xanhtab-session`;
5. clears RAM history and the controller lease;
6. revokes authentication and rotates pairing material.

If any cleanup step fails, all remaining cleanup steps are still attempted and the session moves to `failed`, never falsely to `idle`.

## Public HTTP surface

The machine-readable definition is [`schemas/openapi-v1.yaml`](../schemas/openapi-v1.yaml). It includes pairing exchange, create/status/burn, navigation, egress selection, stream profile, metrics, ticket issuance, and the event WebSocket. Navigation accepts only `http`, `https`, and the internal `xanhtab` scheme. Popup policy remains one-view-only.

## Private service protocols

Both Unix protocols are newline-delimited JSON with a required protocol enum. Browser commands are `start`, `navigate`, and `stop`. Net commands are `apply`, `reset`, and `status`. The privileged helper validates configuration into fixed argv vectors and never accepts an executable name, raw nftables statement, route statement, or shell fragment from the daemon.

## Compatibility

Breaking field or semantic changes require v2. `fireball-docker` must consume released schemas and web-client artifacts instead of copying unversioned source. WebRTC media signaling remains gated by X0 hardware results; the event ticket endpoint must not be mistaken for completed media negotiation.
