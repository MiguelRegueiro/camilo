# lumi

Live camera preview for Kitty-compatible terminals.

This first build is intentionally small:

- Rust app
- `ffmpeg` camera backend
- Kitty graphics protocol
- no UI, no capture, no config file

Run:

```sh
cargo run --release
```

Useful options:

```sh
cargo run --release -- --device /dev/video0 --width 640 --height 360 --fps 30
```

Keys:

- `q`, `Esc`, or `Ctrl-C` exits.

Linux and FreeBSD both use the `v4l2` ffmpeg input in this prototype. On
FreeBSD, that expects a webcam exposed by `webcamd`, usually as `/dev/video0`.
