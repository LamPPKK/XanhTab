# XanhTab Control Surface Overrides

These rules override `../MASTER.md` for the production remote-control surface.

## Product constraints

- Remain fully local and offline-capable. Do not load Google Fonts, animation libraries, icons, or analytics from a CDN; use the local-first Fira-compatible system font stacks in `web/styles.css`.
- Green denotes an available or verified operational state. Red is reserved for destructive Burn Session actions and errors.
- Telemetry may say `LIVE` only after a fresh `/api/v1/metrics` sample. Use `WAITING`, `SYNCING`, `STALE`, or `LOCKED` otherwise.
- Pairing is a mandatory authentication gate. It cannot be dismissed before a successful one-time secret exchange.
- Burn Session always uses its own confirmation dialog, focuses the safe cancel action first, and states that the action revokes the lease and requires new pairing.
- Keep all backend API paths and security semantics unchanged. The visual layer must never introduce mock session or telemetry data into production.

## Interaction rules

- Every interactive control has a minimum 44px touch target and a visible keyboard focus ring.
- Use native `button`, `form`, and `dialog` semantics, plus `aria-pressed` for session policy toggles.
- Do not animate core content into visibility. Motion is limited to status feedback and the decorative scope, with a reduced-motion fallback.
- At 1180px the instrument deck becomes a two-column section grid; at 760px it becomes a single column; at 520px dialogs and destructive summaries stack vertically.

## Visual direction

- Background `#000000`, card `#0C130E`, primary `#00FF41`, destructive `#EF4444`, text `#E0E0E0`, muted `#94A3B8`, border `#1F1F1F`.
- Dense, squared operations-console surfaces; no light theme, glass cards, pill-shaped navigation, or decorative gradients that reduce text contrast.
- Use the scope/radar motif only as a non-interactive isolation indicator. It must never imply live device data.
