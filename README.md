# hypr-rdp

Native RDP server for Hyprland. Connect to your Hyprland desktop from an RDP client.

## Features

- **H.264/EGFX** — AVC420 by default, experimental AVC444 support, and VA-API acceleration with automatic software fallback; clients that never open the graphics pipeline are sent bitmap updates instead
- **Screen capture** — `wlr-screencopy-v1` and `ext-image-copy-capture-v1` protocols
- **Audio** — PipeWire audio forwarding via RDPSND
- **Clipboard** — Bidirectional text and image clipboard sync
- **Input** — Full keyboard and mouse support via virtual keyboard/pointer protocols
- **Session hooks** — Run a command when a client session starts and ends
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
# h264_backend = "auto" # auto, software, or vaapi
# audio_mode = "redirect"
# keyboard_layout_policy = "client"
# output = "DP-1"
# on_session_start = "hyprctl dispatch dpms off eDP-1"  # see "Session hooks"
# on_session_end = "hyprctl dispatch dpms on eDP-1"
```

CLI arguments override config file values.

### Session hooks

`on_session_start` and `on_session_end` run a shell command when a client
session begins and ends:

```toml
on_session_start = "hyprctl dispatch dpms off eDP-1"
on_session_end = "hyprctl dispatch dpms on eDP-1"
```

- Only a fully established session runs a command: port probes, TLS scanners
  and rejected logins never do, and a client resize does not re-run the start
  command. With `-u`/`-p` set the client must pass NLA first; without
  credentials there is no authentication step, so any client that completes
  the RDP handshake runs the command.
- Configured commands run in session order: each waits for the previous one to
  finish, for up to 10 seconds. An unconfigured boundary does not release a
  running command, so a fast reconnect cannot overtake it. Past the deadline
  the previous command is left running, the next one starts alongside it, and
  a warning is logged.
- Stopping hypr-rdp during a session attempts to run the end command and waits
  for it within the same 10-second budget. A command that fails to start,
  exits unsuccessfully, or outlives the deadline cannot guarantee that the
  corresponding start action is undone.
- Commands run through `/bin/sh -c` as the same user as hypr-rdp, with its
  environment and working directory, so `hyprctl` works but shell profiles are
  not read — use absolute paths for anything outside the inherited `PATH`.
  hypr-rdp never kills a command; one still running at exit is left to the
  service manager. Hook command text is not written to hypr-rdp's logs.

Name the monitor when blanking a screen. A bare `hyprctl dispatch dpms off`
also blanks the `hypr-rdp-*` headless output the session is rendered on, which
blanks the session itself. Blanking the captured output is worse still: while
it is off the compositor stops committing frames, so the session freezes, and
remote input wakes it again unless `misc:mouse_move_enables_dpms` and
`misc:key_press_enables_dpms` are disabled.

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--bind`, `-b` | Bind address | `127.0.0.1:3389` |
| `--cert` | TLS certificate (PEM) | Auto-generated |
| `--key` | TLS private key (PEM) | Auto-generated |
| `-u`, `--username` | RDP username | _(none)_ |
| `-p`, `--password` | RDP password | _(none)_ |
| `--resolution`, `-r` | Fixed session resolution, used as-is including above a captured output's size. When omitted for a managed headless output, the session starts at `1920x1080` and may resize to the client-requested size. | Auto client size |
| `--capture-mode` | `wlr` or `ext` | `wlr` |
| `--bitrate` | H.264 bitrate (bps) | `10000000` |
| `--quality` | H.264 quality (0-51) | `23` |
| `--rate-control` | H.264 rate control: `vbr` or `cqp` | `vbr` |
| `--fps` | Max framerate | `30` |
| `--max-frames-in-flight` | Max unacknowledged EGFX frames | `3` |
| `--egfx-codec` | EGFX codec policy: `avc420`, experimental `avc444`, or `auto` | `avc420` |
| `--h264-backend` | H.264 backend: `auto` tries VA-API then software, `software` avoids VA-API, `vaapi` never substitutes software H.264 | `auto` |
| `--audio-mode` | Audio policy: `redirect` routes playback to a temporary RDP sink while connected, `mirror` captures the current sink audio, `off` disables RDPSND | `redirect` |
| `--keyboard-layout-policy` | Keyboard layout policy: `client` applies the RDP client layout; `compositor` keeps the compositor/Hyprland keymap | `client` |
| `--output` | Specific output name. Automatic sizing does not magnify the captured content; one presentation axis may remain larger for letterboxing. | _(headless)_ |
| `--on-session-start` | Shell command run when an authenticated session starts | _(none)_ |
| `--on-session-end` | Shell command run when the session ends | _(none)_ |
| `--config` | Config file path | `~/.config/hypr-rdp/config.toml` |

## License

MIT
