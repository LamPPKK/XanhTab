# X0 encoder evidence — 2026-08-20

This directory contains the publication-safe portion of a real Raspberry Pi Zero 2 W run of `scripts/x0-encoder-probe.sh`.

- Capture time: 2026-08-20T13:57:43Z–13:57:45Z.
- Matrix: 60 frames per profile, 20-second per-profile deadline.
- Result: all four profiles failed while processing the first frame; 720p15 release floor did not pass.
- `summary.json` embeds the read-only preflight and links to the four profile logs.
- Full pre/post dmesg captures were retained only as local diagnostic material. They are not published because a complete boot log is unnecessary for reproducing the gate result; the relevant `ret -3` kernel evidence is recorded in the dated hardware report.

No package, boot configuration, or service was changed during this run.
