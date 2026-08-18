# XanhTab

XanhTab is the Raspberry Pi Zero 2 W remote-browser appliance in the Fireball Browser ecosystem. It exposes one WPE WebKit view to one paired controller over the local network or a private VPN. It has no account system, keeps sensitive session data in tmpfs, and treats Burn Session as a measured security lifecycle rather than a visual reset.

## Current implementation status

The repository contains the executable X0–X2 vertical slice, but **Gate X0 is not yet passed**. A real Pi Zero 2 W preflight on 2026-08-18 found missing `wpesrc`/`webrtcsink` elements and a repeatable V4L2 H.264 failure at the 720p15 release floor. See the [Gate X0 hardware report](docs/hardware-x0-2026-08-18.md). Until the required hardware capture passes `scripts/evaluate-x0.sh`, the installer and dashboard are engineering candidates, not a release.

| Gate | Implemented here | Remaining go/no-go evidence |
| --- | --- | --- |
| X0 | WPE → conversion → V4L2 H.264 → `webrtcsink`, Opus branch, control DataChannel flag, 20-site/three-profile capture harness | Supply `webrtcsink`, resolve the 720p15 encoder failure, then complete the six-hour hardware run and latency/thermal/memory verdict |
| X1 | Three services, pairing, cookie/CSRF, one controller, FSM, tmpfs cleanup, FST blocklist, RAM history, auto-burn | Pi process/socket residue audit and burn SLO measurement |
| X2 | Responsive clientless dashboard, navigation, profile ladder, metrics, five adapter contracts, transactional process restart | Authenticated media signaling bridge and five-adapter IP/DNS/kill-switch hardware matrix |
| X3 | Signed-manifest packaging and verified/idempotent installer structure, repair/uninstall and rollback | Signed first release, fresh-install/upgrade matrix and 24-hour fault soak |

`fireball-docker`, `fireball-webkit`, and `fireball-blink` intentionally remain gated until X3.

## Components

- `xanhtabd`: Rust/Axum TLS control plane and static web client, unprivileged.
- `xanhtab-browser`: dedicated service for the WPE/GStreamer/WebKit process tree, capped at 400 MiB.
- `xanhtab-netd`: minimal privileged egress helper using validated enum commands and fixed argv execution.
- `web/`: industrial, responsive dashboard. The media surface is a native `<video>` element; no frame-by-frame canvas path exists.
- `schemas/`: config v1 JSON Schema and Control API v1 OpenAPI contract.

See [protocol v1](docs/protocol-v1.md) for trust boundaries and lifecycle semantics.

## Local development

Rust 1.85+ and Node are sufficient for the mock-backed control plane:

```sh
cargo test --all-targets
cargo run --bin xanhtabd -- --config config/xanhtab.toml
```

Open `http://127.0.0.1:8088`, then read the development pairing material from `/tmp/xanhtab-pairing.txt`. Development HTTP, mock browser, and mock egress are explicit in `config/xanhtab.toml`; production configuration refuses missing TLS.

## Gate X0 on hardware

On a supported Trixie Pi Zero 2 W, install the required WPE/GStreamer packages and build a release binary. Then run:

```sh
scripts/x0-gate.sh --profile-seconds 7200 --browser-bin target/release/xanhtab-browser
```

The harness runs 20 sites for two hours at each of 1080p30, 720p15, and 480p10 and captures memory, total browser-tree RSS/CPU, temperature, throttling and Wi-Fi signal. Add measured `stream-results.csv` and `latency-results.csv` using the documented headers, then run:

```sh
scripts/evaluate-x0.sh .x0-results/UTC_TIMESTAMP
```

The evaluator only returns GO when 720p15 has less than 2% frame drop, input latency p95 below 250 ms, browser/stream below 400 MiB, at least 48 MiB available, no OOM, and no sustained throttle. If only 480p10 passes, development freezes as required.

## Verified installation

Do not pipe a network response into `sudo`. Download the installer, detached signature, and trusted public key separately; verify before execution:

```sh
curl --fail --location --proto '=https' --tlsv1.2 -O https://github.com/LamPPKK/XanhTab/releases/download/vX.Y.Z/install.sh
curl --fail --location --proto '=https' --tlsv1.2 -O https://github.com/LamPPKK/XanhTab/releases/download/vX.Y.Z/install.sh.minisig
minisign -Vm install.sh -x install.sh.minisig -p ./xanhtab-release.pub
sudo ./install.sh --non-interactive --public-key ./xanhtab-release.pub --network direct
```

The installer stops before mutation on a wrong OS, architecture, or board. It verifies the signed manifest and SHA-256 of the aarch64 artifact, backs up overwritten paths, validates config, starts all services, rolls back on failed health check, and prints the QR only after health succeeds.

For Git-backed public configuration, both `--config-repo` and immutable `--config-ref` are required. Only `config.json`, `custom_hosts.txt`, `bookmarks.json`, and `blocklist-metadata.json` are copied, each with a repository checksum and a 1 MiB limit. Secrets never come from that repository.

`--secrets-file` accepts root-owned mode-0600 JSON with only `wireguard_config_base64` and `proxy_url`. Values are not logged. WARP is encrypted egress, not anonymity; Tor uses remote DNS and a kill-switch contract.

## Security and scope

Container/service hardening is defense in depth, not a promise against every kernel escape or browser zero-day. Burn destroys authentication generations and encryption keys and removes tmpfs state; it does not claim physical overwrite of flash. Packages, non-secret config, the FST blocklist, and redacted security logs remain on disk.

XanhTab v1 does not expose the appliance to the public Internet and does not implement multi-tab, file upload/download, clipboard bridging, DRM/Widevine, WebRTC inside browsed pages, or extensions.
