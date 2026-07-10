use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, BufWriter, ErrorKind, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 360;
const DEFAULT_FPS: u32 = 30;
const DEFAULT_DEVICE: &str = "/dev/video0";
const KITTY_IMAGE_ID: u32 = 0x4c_55_4d; // "LUM", within the 24-bit foreground-color-safe range.
const KITTY_IMAGE_IDS: [u32; 2] = [KITTY_IMAGE_ID, KITTY_IMAGE_ID + 1];
const KITTY_PLACEMENT_ID: u32 = 1;
const RAW_RGB_BYTES_PER_PIXEL: usize = 3;
const KITTY_RAW_CHUNK_BYTES: usize = 3 * 4096 / 4;

#[derive(Clone, Debug)]
struct Config {
    device: String,
    width: u32,
    height: u32,
    fps: u32,
    force: bool,
    mirror_horizontal: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.to_string(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
            force: false,
            mirror_horizontal: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageArea {
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("lumi: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let config = parse_args(env::args().skip(1))?;

    if !config.force && !looks_like_kitty() {
        bail!(
            "this first version targets Kitty graphics; run from kitty or pass --force if your terminal is compatible"
        );
    }

    if inside_tmux() {
        enable_tmux_passthrough();
    }

    let mut camera = CameraStream::spawn(&config)?;
    let frame_len = frame_len(config.width, config.height)?;
    let mut frame = vec![0_u8; frame_len];

    let _terminal = TerminalGuard::enter()?;
    let stop_rx = spawn_input_thread();
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(frame_len + frame_len / 2, stdout.lock());
    let mut kitty_sequence = Vec::with_capacity(frame_len + frame_len / 2 + 4096);
    let mut last_area = None;
    let mut previous_image_id = None;
    let mut frame_serial = 0_u32;

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match camera.read_frame(&mut frame) {
            Ok(true) => {}
            Ok(false) => {
                camera.stop();
                let stderr = camera.stderr_text();
                if stderr.trim().is_empty() {
                    bail!("camera stream ended before a full frame was received");
                }
                bail!("camera stream ended: {}", stderr.trim());
            }
            Err(error) => bail!("failed to read camera frame: {error}"),
        }

        if stop_rx.try_recv().is_ok() {
            break;
        }

        let area = image_area(config.width, config.height);
        if last_area != Some(area) {
            clear_screen_and_images(&mut out)?;
            last_area = Some(area);
            previous_image_id = None;
            frame_serial = 0;
        }

        let image_id = KITTY_IMAGE_IDS[(frame_serial as usize) % KITTY_IMAGE_IDS.len()];
        let z_index = (frame_serial % 1_000_000_000) as i32;
        write_kitty_rgb_frame(
            &mut out,
            image_id,
            KITTY_PLACEMENT_ID,
            z_index,
            previous_image_id,
            config.width,
            config.height,
            area,
            &frame,
            &mut kitty_sequence,
        )?;
        previous_image_id = Some(image_id);
        frame_serial = frame_serial.wrapping_add(1);
        out.flush()?;
    }

    Ok(())
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Config> {
    let args = args.collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help") {
        print_help();
        std::process::exit(0);
    }

    let mut config = Config::default();
    load_config_file(&mut config)?;
    let mut args = args.into_iter().peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-d" | "--device" => {
                config.device = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires a device path"))?;
            }
            "-w" | "--width" => {
                config.width = parse_positive_arg(&arg, args.next())?;
            }
            "-h" | "--height" => {
                config.height = parse_positive_arg(&arg, args.next())?;
            }
            "-f" | "--fps" => {
                config.fps = parse_positive_arg(&arg, args.next())?;
            }
            "--force" => config.force = true,
            "--mirror-horizontal" => config.mirror_horizontal = true,
            _ => bail!("unknown argument: {arg}"),
        }
    }

    if config.width == 0 || config.height == 0 {
        bail!("width and height must be greater than zero");
    }
    if config.fps == 0 || config.fps > 120 {
        bail!("fps must be in the range 1..=120");
    }

    Ok(config)
}

fn load_config_file(config: &mut Config) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read config file {}", path.display()));
        }
    };

    apply_config_text(config, &text)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(())
}

fn apply_config_text(config: &mut Config, text: &str) -> Result<()> {
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_config_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {line_number}: expected `key = value`"))?;
        let key = key.trim();
        let value = value.trim();

        match key {
            "device" => config.device = parse_config_string(value, line_number, key)?,
            "width" => config.width = parse_config_u32(value, line_number, key)?,
            "height" => config.height = parse_config_u32(value, line_number, key)?,
            "fps" => config.fps = parse_config_u32(value, line_number, key)?,
            "force" => config.force = parse_config_bool(value, line_number, key)?,
            "mirror_horizontal" => {
                config.mirror_horizontal = parse_config_bool(value, line_number, key)?;
            }
            "" => bail!("line {line_number}: empty config key"),
            _ => bail!("line {line_number}: unknown config key `{key}`"),
        }
    }
    Ok(())
}

fn strip_config_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(value, _)| value)
}

fn parse_config_string(value: &str, line_number: usize, key: &str) -> Result<String> {
    let Some(body) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        bail!("line {line_number}: `{key}` expects a quoted string");
    };

    Ok(body.to_string())
}

fn parse_config_u32(value: &str, line_number: usize, key: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("line {line_number}: `{key}` expects a positive integer"))
}

fn parse_config_bool(value: &str, line_number: usize, key: &str) -> Result<bool> {
    value
        .parse::<bool>()
        .with_context(|| format!("line {line_number}: `{key}` expects true or false"))
}

fn config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.as_os_str().is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|dir| dir.join("lumi").join("config.toml"))
}

fn parse_positive_arg(flag: &str, value: Option<String>) -> Result<u32> {
    let raw = value.ok_or_else(|| anyhow!("{flag} requires a value"))?;
    raw.parse::<u32>()
        .with_context(|| format!("{flag} expects a positive integer"))
}

fn print_help() {
    println!(
        "\
lumi - live camera preview for Kitty-compatible terminals

Usage:
  lumi [--device /dev/video0] [--width 640] [--height 360] [--fps 30] [--mirror-horizontal] [--force]

Keys:
  q, Esc, Ctrl-C   exit
"
    );
}

fn frame_len(width: u32, height: u32) -> Result<usize> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("frame dimensions are too large"))?;
    pixels
        .checked_mul(RAW_RGB_BYTES_PER_PIXEL as u32)
        .map(|v| v as usize)
        .ok_or_else(|| anyhow!("frame buffer is too large"))
}

fn looks_like_kitty() -> bool {
    env::var("TERM")
        .map(|term| term.to_ascii_lowercase().contains("kitty"))
        .unwrap_or(false)
        || env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var("TERM_PROGRAM")
            .map(|term| term.eq_ignore_ascii_case("kitty"))
            .unwrap_or(false)
}

struct CameraStream {
    child: Child,
    stdout: ChildStdout,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl CameraStream {
    fn spawn(config: &Config) -> Result<Self> {
        let video_filter = ffmpeg_video_filter(config);

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                "v4l2",
                "-i",
                &config.device,
                "-an",
                "-sn",
                "-dn",
                "-vf",
                &video_filter,
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg; is it installed and in PATH?")?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stderr"))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_target = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            let mut stderr_pipe = stderr_pipe;
            let mut text = String::new();
            let _ = stderr_pipe.read_to_string(&mut text);
            if let Ok(mut target) = stderr_target.lock() {
                *target = text;
            }
        });

        Ok(Self {
            child,
            stdout,
            stderr,
            stderr_thread: Some(stderr_thread),
        })
    }

    fn read_frame(&mut self, frame: &mut [u8]) -> io::Result<bool> {
        match self.stdout.read_exact(frame) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CameraStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn ffmpeg_video_filter(config: &Config) -> String {
    let mirror = if config.mirror_horizontal {
        "hflip,"
    } else {
        ""
    };
    format!(
        "{mirror}scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black,fps={},format=rgb24",
        config.width, config.height, config.width, config.height, config.fps
    )
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Clear(ClearType::All), Hide)
            .context("failed to enter terminal preview mode")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write_kitty_apc_bytes(&mut stdout, clear_images_sequence().as_bytes());
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = stdout.flush();
        let _ = disable_raw_mode();
    }
}

fn spawn_input_thread() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key))
                    if key.code == KeyCode::Esc
                        || key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)) =>
                {
                    let _ = tx.send(());
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(());
                    break;
                }
            }
        }
    });
    rx
}

fn image_area(source_width: u32, source_height: u32) -> ImageArea {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (pixel_width, pixel_height) = terminal_pixel_size(cols, rows);
    let cell_width = f64::from(pixel_width) / f64::from(cols.max(1));
    let cell_height = f64::from(pixel_height) / f64::from(rows.max(1));
    let max_width_px = f64::from(cols) * cell_width;
    let max_height_px = f64::from(rows) * cell_height;
    let source_aspect = f64::from(source_width) / f64::from(source_height.max(1));

    let (display_width_px, display_height_px) = if max_width_px / max_height_px > source_aspect {
        (max_height_px * source_aspect, max_height_px)
    } else {
        (max_width_px, max_width_px / source_aspect)
    };

    let display_cols = ((display_width_px / cell_width).floor() as u16).clamp(1, cols.max(1));
    let display_rows = ((display_height_px / cell_height).floor() as u16).clamp(1, rows.max(1));

    ImageArea {
        x: cols.saturating_sub(display_cols) / 2,
        y: rows.saturating_sub(display_rows) / 2,
        cols: display_cols,
        rows: display_rows,
    }
}

fn terminal_pixel_size(cols: u16, rows: u16) -> (u32, u32) {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0;
    if ok {
        let size = unsafe { size.assume_init() };
        if size.ws_xpixel > 0 && size.ws_ypixel > 0 {
            return (u32::from(size.ws_xpixel), u32::from(size.ws_ypixel));
        }
    }

    // Conservative fallback for terminal emulators that do not expose pixel size.
    (u32::from(cols.max(1)) * 8, u32::from(rows.max(1)) * 16)
}

fn clear_screen_and_images(out: &mut impl Write) -> io::Result<()> {
    write_kitty_apc_bytes(out, clear_images_sequence().as_bytes())?;
    out.write_all(b"\x1b[2J\x1b[H")
}

fn clear_images_sequence() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

fn write_kitty_rgb_frame(
    out: &mut impl Write,
    image_id: u32,
    placement_id: u32,
    z_index: i32,
    previous_image_id: Option<u32>,
    width: u32,
    height: u32,
    area: ImageArea,
    frame: &[u8],
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    sequence.clear();
    write!(
        sequence,
        "\x1b[{};{}H",
        area.y.saturating_add(1),
        area.x.saturating_add(1)
    )?;

    let mut offset = 0;
    let mut first = true;
    let mut encoded = [0_u8; 4096];
    while offset < frame.len() {
        let end = (offset + KITTY_RAW_CHUNK_BYTES).min(frame.len());
        let more = end < frame.len();
        let encoded_len = BASE64
            .encode_slice(&frame[offset..end], &mut encoded)
            .map_err(|error| io::Error::new(ErrorKind::Other, error))?;
        if first {
            write!(
                sequence,
                "\x1b_Ga=T,q=2,f=24,s={width},v={height},i={image_id},p={placement_id},c={},r={},C=1,z={z_index},m={};",
                area.cols,
                area.rows,
                if more { 1 } else { 0 },
            )?;
            sequence.extend_from_slice(&encoded[..encoded_len]);
            sequence.extend_from_slice(b"\x1b\\");
            first = false;
        } else {
            write!(sequence, "\x1b_Gm={};", if more { 1 } else { 0 },)?;
            sequence.extend_from_slice(&encoded[..encoded_len]);
            sequence.extend_from_slice(b"\x1b\\");
        }
        offset = end;
    }
    if let Some(previous_image_id) = previous_image_id
        && previous_image_id != image_id
    {
        write!(sequence, "\x1b_Ga=d,d=I,q=2,i={previous_image_id}\x1b\\")?;
    }

    write_kitty_apc_bytes(out, sequence)
}

fn write_kitty_apc_bytes(out: &mut impl Write, sequence: &[u8]) -> io::Result<()> {
    if inside_tmux() {
        out.write_all(&wrap_kitty_apcs_for_tmux(sequence))
    } else {
        out.write_all(sequence)
    }
}

fn inside_tmux() -> bool {
    env::var_os("TMUX").is_some()
}

fn enable_tmux_passthrough() {
    let mut args = vec!["set-option".into(), "-p".into(), "-q".into()];
    if let Some(pane) = env::var_os("TMUX_PANE")
        && !pane.is_empty()
    {
        args.push("-t".into());
        args.push(pane);
    }
    args.push("allow-passthrough".into());
    args.push("on".into());

    let _ = Command::new("tmux")
        .args(args.iter().map(OsStr::new))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wrap_kitty_apcs_for_tmux(sequence: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sequence.len() + sequence.len() / 4);
    let mut i = 0;
    while i < sequence.len() {
        if sequence.len() - i >= 3
            && &sequence[i..i + 3] == b"\x1b_G"
            && let Some(relative_end) = sequence[i + 3..].iter().position(|&byte| byte == 0x1b)
            && sequence.get(i + 3 + relative_end + 1) == Some(&b'\\')
        {
            let body_end = i + 3 + relative_end;
            wrap_sequence_for_tmux(&sequence[i..body_end + 2], &mut out);
            i = body_end + 2;
            continue;
        }
        out.push(sequence[i]);
        i += 1;
    }
    out
}

fn wrap_sequence_for_tmux(sequence: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            out.extend_from_slice(b"\x1b\x1b");
        } else {
            out.push(byte);
        }
    }
    out.extend_from_slice(b"\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_applies_horizontal_mirror() {
        let mut config = Config::default();

        apply_config_text(&mut config, "mirror_horizontal = true\n")
            .expect("config file should parse");

        assert!(config.mirror_horizontal);
        assert_eq!(config.device, DEFAULT_DEVICE);
    }

    #[test]
    fn config_file_parses_comments_strings_and_numbers() {
        let mut config = Config::default();

        apply_config_text(
            &mut config,
            r#"
                # camera settings
                device = "/dev/video1" # inline comment
                width = 800
                height = 450
                fps = 60
            "#,
        )
        .expect("config file should parse");

        assert_eq!(config.device, "/dev/video1");
        assert_eq!(config.width, 800);
        assert_eq!(config.height, 450);
        assert_eq!(config.fps, 60);
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        let mut config = Config::default();

        let error = apply_config_text(&mut config, "unknown = true\n")
            .expect_err("unknown key should fail");

        assert!(error.to_string().contains("unknown config key"));
    }

    #[test]
    fn ffmpeg_filter_adds_hflip_when_horizontal_mirror_is_enabled() {
        let mut config = Config::default();

        assert!(!ffmpeg_video_filter(&config).starts_with("hflip,"));

        config.mirror_horizontal = true;
        assert!(ffmpeg_video_filter(&config).starts_with("hflip,scale="));
    }

    #[test]
    fn kitty_frame_sequence_transmits_raw_rgb_at_requested_area() {
        let frame = [0, 0, 0, 255, 255, 255];
        let area = ImageArea {
            x: 1,
            y: 2,
            cols: 3,
            rows: 4,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(&mut out, 7, 9, 11, None, 2, 1, area, &frame, &mut scratch)
            .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[3;2H"));
        assert!(text.contains("a=T,q=2,f=24,s=2,v=1,i=7,p=9,c=3,r=4,C=1,z=11,m=0;"));
    }

    #[test]
    fn kitty_frame_sequence_chunks_large_frames() {
        let frame = vec![0x7f; KITTY_RAW_CHUNK_BYTES + 1];
        let area = ImageArea {
            x: 0,
            y: 0,
            cols: 10,
            rows: 5,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(
            &mut out,
            7,
            9,
            12,
            None,
            1025,
            1,
            area,
            &frame,
            &mut scratch,
        )
        .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("m=1;"));
        assert!(text.contains("\x1b_Gm=0;") || text.contains("\x1b\x1b_Gm=0;"));
    }

    #[test]
    fn kitty_frame_sequence_deletes_previous_buffer_after_new_frame() {
        let frame = [0, 0, 0, 255, 255, 255];
        let area = ImageArea {
            x: 0,
            y: 0,
            cols: 2,
            rows: 1,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(
            &mut out,
            8,
            9,
            13,
            Some(7),
            2,
            1,
            area,
            &frame,
            &mut scratch,
        )
        .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        let draw = text
            .find("a=T,q=2")
            .expect("new frame draw should be present");
        let delete = text
            .find("a=d,d=I,q=2,i=7")
            .expect("old buffer delete should be present");
        assert!(draw < delete);
    }
}
