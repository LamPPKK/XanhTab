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

Controller authorization has a bounded TTL. A five-second lifecycle watchdog detects when the consumed pairing no longer has a live controller session, serializes against start, egress switch, manual Burn, and inactivity Burn, destroys any abandoned browser state, and publishes a fresh one-time pairing generation. Disabling inactivity auto-burn never makes an expired controller session reusable.

Event and WebRTC signaling connections use purpose-bound, one-time, short-lived tickets from `POST /api/v1/webrtc/ticket`. Tickets never appear in the WebSocket URL: the client must send `{"type":"authenticate","ticket":"…"}` as its first text frame within five seconds. A ticket issued for `events` cannot open `signaling`, and either use consumes it. Burn increments the auth generation and invalidates every cookie and outstanding ticket.

## Session state

The state machine is `idle → starting → active → burning → idle`; an observable `failed` state records redacted lifecycle failures. Only the paired client can hold the controller lease. A successful burn:

1. marks the session `burning` and closes event delivery;
2. stops the complete browser/GStreamer cgroup;
3. restores the default egress policy;
4. clears `/run/xanhtab-session`;
5. clears RAM history and the controller lease;
6. revokes authentication and rotates pairing material.

If any cleanup step fails, all remaining cleanup steps are still attempted and the session moves to `failed`, never falsely to `idle`.

The signed appliance archive includes `xanhtab-x1-burn-audit`. It exercises this lifecycle through the public API and observes the browser service cgroup before and after Burn. Its schema-v1 JSON output is ephemeral under `/run`, redacts all session material, and only passes after a session process was observed, the frozen pre-burn cookie is rejected, pairing rotates, runtime residue is zero, no session process remains, the phase is `idle`, and Burn completes below the five-second SLO. Authentication material is kept in root-only files rather than argv; an interrupted audit attempts an emergency Burn and preserves recovery material only if cleanup cannot be confirmed. Ticket revocation remains covered by the control-plane integration test because the audit never writes a WebSocket ticket to its report.

## Public HTTP surface

The machine-readable definition is [`schemas/openapi-v1.yaml`](../schemas/openapi-v1.yaml). It includes pairing exchange, create/status/burn, navigation, egress selection, stream profile, per-session blocklist and auto-burn settings, versioned metrics, purpose-bound ticket issuance, the event WebSocket, and the authenticated signaling WebSocket. Navigation accepts only `http`, `https`, and the internal `xanhtab` scheme. Popup policy remains one-view-only.

`/ws/v1/session/{id}/signal` validates same-origin metadata, upgrades without URL credentials, consumes a `signaling` ticket from the time-limited first frame, checks the active controller lease, and only then connects to the configured loopback rswebrtc server. The embedded rswebrtc web server is disabled. Its signaling listener binds `127.0.0.1:8444`, and the public STUN default is explicitly blank unless an operator opts in through a future reviewed configuration path.

## Private service protocols

Both Unix protocols are newline-delimited JSON with a required protocol enum. Browser commands are `start`, `navigate`, and `stop`. Net commands are `apply`, `reset`, and `status`. The privileged helper derives one complete nftables transaction from validated local configuration; it never accepts an executable name, raw nftables statement, route statement, or shell fragment from the daemon.

## Compatibility

Breaking field or semantic changes require v2. `fireball-docker` must consume released schemas and web-client artifacts instead of copying unversioned source. The signaling relay contract is implemented, but end-to-end rswebrtc consumer negotiation and media/input evidence remain gated by X0 hardware results.
