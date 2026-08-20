# XanhTab Gate X0 Remediation Runbook

## Purpose and current stop condition

This runbook is for a maintainer recovering Gate X0 on the supported Raspberry Pi Zero 2 W target. It is not an installer and does not authorize a system change by itself.

The 2026-08-20 device is currently stopped for two independent reasons:

- All four bounded H.264 probes returned `frame-processing-error`; the kernel logged `bcm2835_codec_start_streaming: Failed enabling i/p port, ret -3`.
- After repeated failed probes, `CmaFree` was observed at 2,212 KiB and then 3,472 KiB with no process holding `/dev/video11`. This does not prove a kernel leak, but it is below the harness safety floor and forbids another encoder attempt before a controlled reboot.

The same `ret -3` symptom exists in [raspberrypi/linux issue #3974](https://github.com/raspberrypi/linux/issues/3974), but that issue does not establish a root cause or fix. Its reporter also found that increasing `gpu_mem` and trying multiple kernel/firmware pairs did not solve their case. Do not label this device failure as “insufficient CMA”, “bad firmware”, or “bad hardware” without a controlled comparison.

## Prerequisites

- Explicit approval for each reboot, boot-file edit, or package mutation.
- Physical access to the Pi and its boot media in case SSH does not return.
- A second high-quality microSD for the clean-image comparison. Do not use the only recoverable card for kernel experimentation.
- A verified checkout containing `scripts/x0-preflight-json.sh` and `scripts/x0-encoder-probe.sh`.
- Root access for reboot/package steps; the evidence scripts themselves should run as the normal `pi` user in the `video` group.
- Current evidence retained under [`benchmarks/x0-encoder-2026-08-20`](../benchmarks/x0-encoder-2026-08-20/README.md).

Never use `rpi-update` in this workflow. Raspberry Pi documents it as a pre-release developer tool that can make a system unstable or unbootable; stable kernel and firmware updates should come from APT unless a Raspberry Pi engineer explicitly instructs otherwise. See the official [Raspberry Pi OS update guidance](https://www.raspberrypi.com/documentation/computers/os.html#update-software).

## 1. Freeze the baseline

1. Confirm no probe or browser process is running:

   ```sh
   fuser /dev/video11 || true
   ps -eo pid,comm,args | grep -E 'gst-launch|xanhtab' | grep -v grep || true
   ```

2. Capture the read-only state:

   ```sh
   scripts/x0-preflight-json.sh | jq .
   scripts/x0-encoder-probe.sh --dry-run
   ```

3. Verify that the probe refuses to proceed when `CmaFree` is below 32 MiB. Do not bypass this check.

Current package baseline:

| Package | Installed | Candidate observed without `apt update` |
| --- | --- | --- |
| `linux-image-rpi-v8` | `1:6.18.34-1+rpt1` | `1:6.18.39-1+rpt1` |
| `raspi-firmware` | `1:1.20260521-3` | same |
| `gstreamer1.0-tools` | `1.26.2-2` | same |
| `gstreamer1.0-wpe` | not installed | `1.26.2-3+rpt3+deb13u2` |
| `libwpewebkit-2.0-1` | not installed | `2.48.3-1` |

Candidate values are a snapshot of the device’s existing APT cache, not a version pin or an instruction to install.

## 2. Reboot-only recovery lane

This is the first mutation to request approval for. Do not edit boot files or update packages in the same lane.

1. Record `CmaFree`, temperature, throttling and the current preflight JSON.
2. With approval and physical recovery available, reboot:

   ```sh
   sudo reboot
   ```

3. Reconnect and immediately verify:

   ```sh
   awk '/^(MemAvailable|CmaTotal|CmaFree):/ {print}' /proc/meminfo
   vcgencmd get_throttled
   scripts/x0-preflight-json.sh | jq '{captured_at, hardware, cgroups, packages}'
   ```

4. Continue only if `CmaFree` is at least 32 MiB, `get_throttled=0x0`, and no process holds `/dev/video11`.
5. Run the safe default probe once:

   ```sh
   scripts/x0-encoder-probe.sh
   ```

The default stops at the first failed profile. Do not add `--continue-after-failure` unless the run is explicitly approved as a diagnostic matrix and the before/after CMA values remain above the floor.

### Verification

- A successful lane must produce `summary.json`, per-profile logs and passing `SHA256SUMS`.
- Exit code zero only proves that 720p15 encoded; it does not prove Gate X0.
- If 480p10 fails again or CMA drops below the safety floor, stop and retain the bundle.

### Rollback and escalation

A reboot-only lane changes no persistent file. If SSH does not return, use physical console/power recovery and do not continue remotely. If `CmaFree` remains below 32 MiB after a clean reboot, capture preflight only and escalate upstream; do not run another encoder pipeline.

## 3. Memory-cgroup lane

This lane is independent of the encoder failure. It exists because the production units rely on `MemoryHigh`/`MemoryMax`.

The active `/boot/firmware/cmdline.txt` does **not** contain `cgroup_disable=memory`; that token is injected earlier into `/proc/cmdline`. The boot file also lacks `cgroup_memory=1` and `cgroup_enable=memory`. A report on the official [RPi-Distro/pi-gen tracker](https://github.com/RPi-Distro/pi-gen/issues/917) demonstrates those two enable tokens on Trixie arm64, but it is evidence from an issue report, not a support guarantee for this exact image.

1. Verify that `cmdline.txt` contains exactly one line and neither enable token:

   ```sh
   test "$(wc -l < /boot/firmware/cmdline.txt)" -eq 1
   ! grep -Eq '(^| )cgroup_(memory=1|enable=memory)( |$)' /boot/firmware/cmdline.txt
   ```

2. Create one metadata-preserving backup at the fixed rollback path. Stop if that path already exists; never overwrite the recovery copy:

   ```sh
   xanhtab_backup=/boot/firmware/cmdline.txt.xanhtab-pre-memcg.bak
   test ! -e "$xanhtab_backup"
   sudo cp -a -- /boot/firmware/cmdline.txt "$xanhtab_backup"
   sudo sha256sum /boot/firmware/cmdline.txt "$xanhtab_backup"
   ```

3. With separate approval for the edit, append both tokens to the existing single line:

   ```sh
   sudo sed -i '1 s/[[:space:]]*$/ cgroup_memory=1 cgroup_enable=memory/' /boot/firmware/cmdline.txt
   ```

4. Verify the file before reboot:

   ```sh
   test "$(wc -l < /boot/firmware/cmdline.txt)" -eq 1
   grep -Eq '(^| )cgroup_memory=1( |$)' /boot/firmware/cmdline.txt
   grep -Eq '(^| )cgroup_enable=memory( |$)' /boot/firmware/cmdline.txt
   ```

5. Reboot only after the fixed backup path and checksum have been recorded out-of-band.

### Verification

After reboot, the controller list—not the text of `/proc/cmdline`—is authoritative:

```sh
grep -qw memory /sys/fs/cgroup/cgroup.controllers
scripts/x0-preflight-json.sh | jq '.cgroups'
systemctl show xanhtab-browser.service -p MemoryHigh -p MemoryMax 2>/dev/null || true
```

The lane passes only when `memory_controller` is true and both boot-file intent fields are true. Record `MemAvailable`; memory accounting has overhead on a 512 MiB board and the X0 budget must be remeasured.

### Rollback and escalation

If the controller is still absent, boot stability regresses, or available memory becomes unacceptable, restore the exact recorded backup and reboot:

```sh
sudo cp -a -- /boot/firmware/cmdline.txt.xanhtab-pre-memcg.bak /boot/firmware/cmdline.txt
sudo reboot
```

Verify the backup checksum against the out-of-band record before restoring it. Do not substitute another `.bak` file.

## 4. Clean-image kernel/firmware comparison

Do this on the second microSD, not by making the current evidence card harder to recover.

1. Flash a fresh Raspberry Pi OS Lite 64-bit Trixie image. Raspberry Pi documents `/boot/firmware/` as the boot partition mount and `cmdline.txt` as the kernel command line source; see the official [configuration reference](https://www.raspberrypi.com/documentation/computers/configuration.html).
2. Boot without restoring application configuration.
3. Capture preflight and run the safe default encoder probe before any package update.
4. If the same `ret -3` occurs, retain the bundle as the clean-image control.
5. Only with approval, follow Raspberry Pi’s stable APT path on this test card:

   ```sh
   sudo apt update
   sudo apt full-upgrade
   sudo reboot
   ```

6. Capture package versions and rerun the safe probe.

### Verification

Compare exact kernel, `raspi-firmware`, GStreamer, CMA-before/after and profile results. Do not attribute a change to the kernel if firmware or GStreamer changed in the same APT transaction; record the whole package delta.

### Rollback and escalation

Rollback is switching back to the untouched original card. Do not use `rpi-update` or attempt an unverified kernel downgrade. If current stable Trixie on a clean card still returns `ret -3`, file/update an upstream Raspberry Pi issue with the redacted bundle and a link to #3974.

## 5. WPE and rswebrtc lane

Do not enter this lane until the standalone 720p15 encoder probe passes.

1. On the test card, install supported WPE packages only after approval:

   ```sh
   sudo apt install --no-install-recommends gstreamer1.0-wpe libwpewebkit-2.0-1
   ```

2. Verify `wpesrc` and record exact installed versions:

   ```sh
   gst-inspect-1.0 wpesrc
   dpkg-query -W gstreamer1.0-wpe libwpewebkit-2.0-1
   ```

3. Supply `webrtcsink` only from the pinned ARM64 artifact covered by the signed XanhTab release manifest. Do not fetch an unsigned plugin or assume an APT package exists.
4. Only then run the WPE → conversion → V4L2 H.264 → `webrtcsink` integration and control DataChannel checks.

### Verification

Gate X0 still requires the full 20-site, two-hour-per-profile hardware capture and `scripts/evaluate-x0.sh`. A working plugin inspection or short encoder probe is only a precondition.

### Rollback and escalation

Discard/reflash the test card if the package experiment diverges from the supported image. Do not promote package state from an ad hoc test card into the installer until versions, checksums, ABI and signed-manifest coverage are fixed.

## Stop rules

- Stop immediately below 32 MiB `CmaFree`.
- Stop after the first failed encoder profile unless a controlled full matrix was explicitly approved.
- Stop if memory cgroup cannot be made authoritative after rollback-safe boot testing.
- Stop if an artifact lacks a pinned version, SHA-256 and valid release-manifest signature.
- Never claim the cause of `ret -3` without a controlled result that distinguishes image, kernel, firmware, memory pressure and pipeline negotiation.
