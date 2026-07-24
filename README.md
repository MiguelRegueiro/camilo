<h1 align="left"><img src="assets/logo.png" width="64" alt="camilo logo" align="absmiddle" />&nbsp;camilo</h1>

Terminal camera app for Kitty.

Take photos, record videos, and control the camera from a terminal UI.

## Features

- **Photo and video capture** — switch modes from the sidebar and use the shutter control
- **Live preview** — low-latency camera feed with optional horizontal mirroring
- **Audio recording** — use the default microphone or choose an input explicitly
- **Camera settings** — choose device, resolution, FPS, output folder, and preview backend
- **Self-timer and thumbnails** — countdown capture plus latest-capture preview

## Requirements

- Kitty
- V4L2 camera device
- FFmpeg for fallback preview and recording

On FreeBSD, expose webcams with `webcamd`, usually as `/dev/video0`.

## Run from source

Install Rust 1.96+ and the native camera/media dependencies, then run:

```sh
cargo run --release
```

## CLI

```text
camilo [options]
```

Flags:

- `--device <path>` — select the V4L2 camera device
- `--width <px>` / `--height <px>` / `--fps <n>` — set capture size and frame rate
- `--preview-backend <auto|v4l2|ffmpeg>` — choose direct V4L2 preview or ffmpeg fallback
- `--camera-dir <path>` — choose where photos and videos are saved
- `--mirror-horizontal` / `--no-mirror-horizontal` — control preview mirroring
- `--audio-input <name>` — choose the microphone/input used for video recording
- `--no-audio` — record video without audio
- `--camera-info` — print selected camera mode information and exit
- `--force` — run on compatible terminals that do not advertise themselves as Kitty

## Config

camilo reads configuration from `~/.config/camilo/config.toml`:

```toml
camera_dir = "~/Pictures/Camera"
# device = "/dev/video0"
# width = 1920
# height = 1080
# fps = 30
# preview_backend = "auto" # auto | v4l2 | ffmpeg
# mirror_horizontal = true
# audio = true
# audio_input = "default"
```

<details>
<summary><strong>Controls</strong></summary>

- Click the shutter button to take a photo or start/stop video recording
- Click the mode switch to toggle photo/video mode
- Click the timer control to cycle the self-timer
- Press `q` to quit

</details>
