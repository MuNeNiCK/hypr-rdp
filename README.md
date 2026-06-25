# hypr-rdp

Native RDP server for Hyprland. Connect to your Hyprland desktop from an RDP client.

## Features

- **H.264/EGFX** — AVC420 by default, experimental AVC444 support, and VA-API acceleration with automatic software fallback
- **Screen capture** — `wlr-screencopy-v1` and `ext-image-copy-capture-v1` protocols
- **Audio** — PipeWire audio forwarding via RDPSND
- **Clipboard** — Bidirectional text and image clipboard sync
- **Input** — Full keyboard and mouse support via virtual keyboard/pointer protocols
- **TLS** — Auto-generated self-signed certificates, or bring your own
- **Config file** — `~/.config/hypr-rdp/config.toml`

## Installation

### AUR (Arch Linux)

```sh
# Stable release
yay -S hypr-rdp

# Latest git build
yay -S hypr-rdp-git
```

### Nix

```sh
# Run from GitHub
nix run github:MuNeNICK/hypr-rdp#hypr-rdp -- --help

# Build from GitHub
nix build github:MuNeNICK/hypr-rdp#hypr-rdp

# Development shell
nix develop github:MuNeNICK/hypr-rdp#hypr-rdp
```

### Prebuilt binary

Download from [GitHub Releases](https://github.com/MuNeNICK/hypr-rdp/releases):

```sh
tar xzf hypr-rdp-v*.tar.gz
sudo install -Dm755 hypr-rdp /usr/local/bin/hypr-rdp
```

Runtime dependencies: `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon`,
and `pactl` through PipeWire's PulseAudio compatibility layer for the default
remote-audio routing mode.

For VA-API hardware encoding, install a VA-API driver such as
`intel-media-driver` for Intel GPUs or `libva-mesa-driver` for AMD GPUs.

### Build from source

Requirements:
- Rust 1.75+
- `ffmpeg`/`libavcodec`, `libva`, `pipewire`, `libxkbcommon` (development headers)

```sh
git clone https://github.com/MuNeNICK/hypr-rdp.git
cd hypr-rdp
cargo build --release
sudo install -Dm755 target/release/hypr-rdp /usr/local/bin/hypr-rdp
```

## Usage

Requires **Hyprland 0.54+**.
VA-API is included in the standard build and falls back to software encoding
automatically when unavailable.

```sh
# Basic (auto-generates TLS cert, binds to 127.0.0.1:3389)
hypr-rdp -u <username> -p <password>

# Bind to all interfaces
hypr-rdp -u user -p pass --bind 0.0.0.0:3389

# Custom resolution and framerate
hypr-rdp -u user -p pass --resolution 2560x1440 --fps 60

# Capture a specific output
hypr-rdp -u user -p pass --output DP-1

# Use ext-image-copy-capture protocol
hypr-rdp -u user -p pass --capture-mode ext
```

### Config file

`~/.config/hypr-rdp/config.toml`:

```toml
bind = "0.0.0.0:3389"
username = "user"
password = "pass"
# resolution = "1920x1080"
capture_mode = "wlr"
bitrate = 10000000
quality = 23
fps = 30
egfx_codec = "avc420"
# audio_mode = "redirect"
# keyboard_layout_policy = "client"
# output = "DP-1"
```

CLI arguments override config file values.

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--bind`, `-b` | Bind address | `127.0.0.1:3389` |
| `--cert` | TLS certificate (PEM) | Auto-generated |
| `--key` | TLS private key (PEM) | Auto-generated |
| `-u`, `--username` | RDP username | _(none)_ |
| `-p`, `--password` | RDP password | _(none)_ |
| `--resolution`, `-r` | Fixed session resolution. When omitted for a managed headless output, the session starts at `1920x1080` and may resize to the client-requested size. | Auto client size |
| `--capture-mode` | `wlr` or `ext` | `wlr` |
| `--bitrate` | H.264 bitrate (bps) | `10000000` |
| `--quality` | H.264 quality (0-51) | `23` |
| `--rate-control` | H.264 rate control: `vbr` or `cqp` | `vbr` |
| `--fps` | Max framerate | `30` |
| `--max-frames-in-flight` | Max unacknowledged EGFX frames | `3` |
| `--egfx-codec` | EGFX codec policy: `avc420`, experimental `avc444`, or `auto` | `avc420` |
| `--audio-mode` | Audio policy: `redirect` routes playback to a temporary RDP sink while connected, `mirror` captures the current sink audio, `off` disables RDPSND | `redirect` |
| `--keyboard-layout-policy` | Keyboard layout policy: `client` applies the RDP client layout; `compositor` keeps the compositor/Hyprland keymap | `client` |
| `--output` | Specific output name | _(headless)_ |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |

## License

MIT
