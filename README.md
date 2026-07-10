# lumi

Live camera preview for Kitty-compatible terminals.

This first build is intentionally small:

- Rust app
- `ffmpeg` camera backend
- Kitty graphics protocol
- right-side capture controls

Run:

```sh
cargo run --release
```

Useful options:

```sh
cargo run --release -- --device /dev/video0 --width 640 --height 480 --fps 30
```

Optional config:

```toml
# ~/.config/lumi/config.toml
mirror_horizontal = true
camera_dir = "~/Pictures/Camera"
# device = "/dev/video0"
# width = 640
# height = 480
# fps = 30
```

To mirror only for one run:

```sh
cargo run --release -- --mirror-horizontal
```

Keys:

- `Space`, `Enter`, or the right-side shutter button takes a picture.
- `q`, `Esc`, or `Ctrl-C` exits.

Linux and FreeBSD both use the `v4l2` ffmpeg input in this prototype. On
FreeBSD, that expects a webcam exposed by `webcamd`, usually as `/dev/video0`.
