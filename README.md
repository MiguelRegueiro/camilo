# camilo

Camera app for the terminal.

Camilo uses `ffmpeg` for camera input and the Kitty graphics protocol for the live preview, shutter controls, photo capture, and video recording.

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
cargo run --release -- --device /dev/video0 --width 1920 --height 1080 --fps 30
```

## Config

```toml
# ~/.config/camilo/config.toml
mirror_horizontal = true
camera_dir = "~/Pictures/Camera"
# device = "/dev/video0"
# width = 1920
# height = 1080
# fps = 30
# audio = true
# audio_input = "default"
```

To mirror only for one run:

```sh
cargo run --release -- --mirror-horizontal
```

Audio is recorded from the system default microphone. Set `audio_input` to override it, or disable audio for one run:

```sh
cargo run --release -- --no-audio
```

## Controls

- Use the right-side shutter button to take pictures or start/stop video recording.
- Use the right-side mode switch to toggle photo/video mode.
- Press `q` to quit.

Linux and FreeBSD both use the `v4l2` ffmpeg input. On FreeBSD, that expects a webcam exposed by `webcamd`, usually as `/dev/video0`.
