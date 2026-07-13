<h1 align="left"><img src="assets/logo.png" width="64" alt="camilo logo" align="absmiddle" />&nbsp;camilo</h1>

<p>
  A camera app for the terminal.
</p>

Camilo uses direct V4L2 preview on Linux and FreeBSD, `ffmpeg` as fallback/encoder, and the Kitty graphics protocol for the live preview, shutter controls, photo capture, and video recording.

## Terminal Compatibility

| Terminal | Status | Notes |
| --- | --- | --- |
| Kitty | Supported | Recommended and tested target. |

## Run

```sh
cargo run --release
```

Useful options:

```sh
cargo run --release -- --device /dev/video0 --width 1920 --height 1080 --fps 30 --preview-backend auto
cargo run --release -- --camera-info
```

Preview backends:

| Backend | Behavior |
| --- | --- |
| `auto` | Try direct V4L2 first, fall back to `ffmpeg`. |
| `v4l2` | Force direct V4L2 preview. |
| `ffmpeg` | Force the fallback preview path. |

## Config

```toml
# ~/.config/camilo/config.toml
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

Preview is mirrored horizontally by default. To disable mirroring only for one run:

```sh
cargo run --release -- --no-mirror-horizontal
```

Audio is recorded from the system default microphone. Set `audio_input` to override it, or disable audio for one run:

```sh
cargo run --release -- --no-audio
```

## Controls

- Use the right-side shutter button to take pictures or start/stop video recording.
- Use the right-side mode switch to toggle photo/video mode.
- Press `q` to quit.

Linux and FreeBSD both use V4L2 devices. On FreeBSD, V4L2 expects a webcam exposed by `webcamd`, usually as `/dev/video0`.
