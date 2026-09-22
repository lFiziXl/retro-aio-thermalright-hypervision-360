<div align="center">

# ⚡ RETRO AIO

### The mascot daemon for your Thermalright Hyper Vision 360 ARGB LCD cooler.

**One small Rust binary. ~0% CPU. ~5 MB RAM. Zero Electron. Zero vendor bloat.**

A retro-futuristic companion that watches your hardware — and *actually reacts* when it gets hot.

[![License: GPL-3.0](https://img.shields.io/badge/License-GPLv3-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Linux-green.svg)](https://www.linux.org/)

</div>

---

## 🖥 What is this?

Your AIO liquid cooler has a screen. Most people waste it on a static logo — or install a 400 MB Electron app that burns a whole CPU core just to render a spinning GIF.

**retro_aio** turns your Thermalright panel into a living cyberpunk dashboard. It features a pixel mascot framed in a cyan CRT-style terminal — complete with scanlines, targeting brackets, and a Matrix digital rain background. Everything is drawn pixel by pixel at a capped ~30 FPS, with no heavy image pipeline, no compositor, and no excuses.

## 📦 Why it's fast

| | Typical LCD Software | **retro_aio** |
|---|---|---|
| Runtime | Electron / Node | Single Rust static binary |
| CPU at idle | 1–5% | **~0%** |
| RAM | 150–400 MB | **~5 MB** |
| Art | Shipped PNGs + JS framework | Programmatic, compiled into the binary |
| Telemetry | Heavy vendor daemon + IPC | Direct `sysfs` / `nvidia-smi` |

- **No Electron.** No Chromium, no Node, no gigabytes of RAM wasted to draw a square.
- **No vendor software.** No bloated manufacturer daemons, no Windows-only control apps ported with a band-aid.
- **Zero-asset visuals.** The CRT frame, scanlines, brackets, and digital rain are rendered purely through code at runtime (no external image files required).

## ✨ Features

### 📡 Telemetry
**Displayed on screen:**
- **CPU:** Temperature and Frequency.
- **GPU:** Temperature and VRAM usage.

**Tracked in the background (to drive mascot states):**
- **GPU Load:** Supports both AMD (via `amdgpu` sysfs) and NVIDIA (via `nvidia-smi`). The daemon takes the *max* of both sources, so hybrid laptops/rigs report honestly.
- **RAM Usage:** Tracks the Used/Total ratio.
- Sensor discovery is driver-based, avoiding fragile `hwmonN` index shifts after Linux kernel updates.

### 🌐 Cyberpunk Visuals
- Cyan HUD frame: 1px border inset with targeting-bracket corners — 100% drawn by code.
- CRT scanlines for authentic phosphor-glow aesthetics.
- **Matrix digital rain background** — cyan glyphs fall, wrap, and reset. Written straight into the framebuffer via pixel manipulation for absolute zero performance overhead.

### 🤖 Dynamic State Machine
The mascot is a deterministic mood machine:

| State | Trigger | Behavior |
|---|---|---|
| `IDLE` | Normal Load | `normal`, `smoke`, or `squints` poses (changes every 30–240s) |
| `SMILE` | GPU Load 50–79% | A pleased grin — you're doing fine, keep going. |
| `SCREAM` | **GPU Load ≥ 80% or RAM ≥ 60%** | Strict 10s lock: `angry_scream` sprite + a sarcastic quote. No flickering even if load drops for a tick. |
| `ANGRY` | Still overloaded after scream | Silent glare until the next quote cycle (~6 min cooldown). |

*Quotes are compiled into the binary at build time. No LLMs, no network requests, no delays.*

### 🌍 Smart Localization
Quotes automatically switch between **English** and **Russian** based on your host OS locale (`sys-locale`). Set your system language once and the mascot adapts instantly.

## 🚀 Installation

> One command. That's it. The script installs Rust (if missing), compiles the binary tailored to your machine, sets up a background service, and cleans up after itself.

```bash
curl -sSL [https://raw.githubusercontent.com/lFiziXl/retro-aio-thermalright-hypervision-360/main/install.sh](https://raw.githubusercontent.com/lFiziXl/retro-aio-thermalright-hypervision-360/main/install.sh) | bash
Manage your mascot via systemd:
Since the daemon runs as a user service (no root required), manage it with the --user flag:

Bash
systemctl --user status retro_aio      # Check if the mascot is alive
journalctl --user -u retro_aio -f      # Read the daemon logs
systemctl --user restart retro_aio     # Restart the daemon
🗑️ Uninstallation
To completely stop and remove retro_aio from your system, run:

Bash
systemctl --user stop retro_aio.service
systemctl --user disable retro_aio.service
rm ~/.config/systemd/user/retro_aio.service
rm ~/.local/bin/retro_aio
systemctl --user daemon-reload
🔌 Hardware Notes & Troubleshooting
Target Device: Designed specifically for the Thermalright Hyper Vision 360 ARGB (and identical ChiZhu Tech AIO panels: 87ad:70db) using the USBLCDNew raw-bulk USB protocol.

Permissions: Since this runs as a user service, your user needs permission to write to the USB device. If the daemon fails to connect, you must add a standard udev rule for your cooler's USB vendor ID.

OS: Linux only.

🏗 Architecture
Plaintext
retro_aio/
├── assets/          # Sprites, font, quotes (compiled straight into the binary)
├── src/
│   ├── main.rs      # 30 FPS render loop, state machine, quote logic
│   ├── render.rs    # Programmatic frame, Matrix rain, scanlines, compositing
│   ├── screen.rs    # USB driver: handshake, JPEG framing, bulk writes
│   └── telemetry.rs # Sysfs / nvidia-smi / sysinfo hardware polling
└── install.sh       # One-line automated setup
🙏 Credits
USB Protocol Reverse-Engineering: Huge thanks to Lexonight1 (Link to their GitHub) for originally reverse-engineering the USBLCDNew raw-bulk protocol. Without their groundwork, this panel would just be an expensive paperweight.

Built with standard-setting Rust crates: rusb, sysinfo, image, imageproc, ab_glyph, and sys-locale.

⚖️ License
Distributed under the GNU GPLv3 — see LICENSE.
Do what you want, share alike. If you ship it, you share it.
