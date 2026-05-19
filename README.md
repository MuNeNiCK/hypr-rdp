# hypr-rdp

Native RDP server for Hyprland. Connect to your Hyprland desktop from an RDP client.

## Features

- **H.264/EGFX** — OpenH264 software encoding by default, with optional VA-API hardware encoding
- **Screen capture** — `wlr-screencopy-v1` and `ext-image-copy-capture-v1` protocols
- **Audio** — PipeWire audio forwarding via RDPSND
- **Clipboard** — Bidirectional text and image clipboard sync
- **Input** — Full keyboard and mouse support via virtual keyboard/pointer protocols
- **TLS** — Auto-generated self-signed certificates, or bring your own
- **Config file** — `~/.config/hypr-rdp/config.toml`

## Installation

### AUR (Arch Linux)

```sh
# Latest git (recommended)
yay -S hypr-rdp-git

# Stable release
yay -S hypr-rdp
```

### Prebuilt binary

Download from [GitHub Releases](https://github.com/MuNeNICK/hypr-rdp/releases):

```sh
tar xzf hypr-rdp-v*.tar.gz
sudo install -Dm755 hypr-rdp /usr/local/bin/hypr-rdp
```

Runtime dependencies: `openh264`, `pipewire`, `libxkbcommon`

### Build from source

Requirements:
- Rust 1.75+
- `pipewire`, `libxkbcommon` (development headers)
- `openh264` at runtime

```sh
git clone https://github.com/MuNeNICK/hypr-rdp.git
cd hypr-rdp
cargo build --release
sudo install -Dm755 target/release/hypr-rdp /usr/local/bin/hypr-rdp
```

VA-API build (optional hardware encoding):

```sh
cargo build --release --features vaapi
```

## Usage

Requires **Hyprland 0.54+**. VA-API builds additionally need `libva` and a VA-API driver (`intel-media-driver` for Intel, `libva-mesa-driver` for AMD).

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
hypr-rdp -u user -p pass --capture_mode ext
```

### Config file

`~/.config/hypr-rdp/config.toml`:

```toml
bind = "0.0.0.0:3389"
username = "user"
password = "pass"
resolution = "1920x1080"
capture_mode = "wlr"
bitrate = 5000000
quality = 23
fps = 30
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
| `--resolution`, `-r` | Session resolution | `1920x1080` |
| `--capture_mode` | `wlr` or `ext` | `wlr` |
| `--bitrate` | H.264 bitrate (bps) | `5000000` |
| `--quality` | H.264 quality (0-51) | `23` |
| `--fps` | Max framerate | `30` |
| `--output` | Specific output name | _(headless)_ |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |

## License

MIT
