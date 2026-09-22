# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-09-21

### Added

- Initial public release of `camilo`.
- Terminal camera interface for Kitty, with live preview, mouse controls, and a photographic shutter-style UI.
- Photo capture and video recording, with a photo/video mode switch and responsive capture controls.
- Microphone audio recording, explicit audio-input selection, and a `--no-audio` mode for silent video capture.
- Camera device, resolution, and frame-rate selection, including native selection of the best supported V4L2 camera mode.
- Direct V4L2 preview on Linux and FreeBSD, with an FFmpeg preview fallback and selectable preview backend.
- Optional horizontal preview mirroring, enabled by default.
- Configurable picture and video output directories, using XDG media directories by default.
- Self-timer countdown capture and a fullscreen gallery for browsing recent photos.
- Command-line controls for camera selection, output paths, capture dimensions, frame rate, preview backend, audio, mirroring, camera information, and terminal detection override.
- Kitty graphics passthrough in tmux, including shared-memory frame transfer for smooth previews.
- Linux desktop entry and hicolor application icons for packaged installs.
- FreeBSD camera support through webcamd-managed V4L2 devices.
- Optimized release builds, efficient camera-frame processing, and low-latency previews that remain responsive while video encoding.
- Reliable V4L2 preview startup, shutdown, transient-read handling, and stalled-stream recovery.
- Accurate recording duration, asynchronous recording finalization, stable Kitty graphics stacking, and correctly sized preview and timer layouts.

[Unreleased]: https://github.com/MiguelRegueiro/camilo/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/MiguelRegueiro/camilo/releases/tag/v1.0.0
