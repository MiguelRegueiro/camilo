#![cfg(target_os = "freebsd")]

use std::{
    ffi::CString,
    io::{self, ErrorKind},
    mem,
    os::raw::{c_char, c_int, c_ulong, c_void},
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};

use crate::config::Config;

use super::{
    camera_modes::{CameraMode, camera_mode_preference, fps_from_interval},
    camera_stream::{
        LatestCameraFrame, V4l2FrameConfig, V4l2StreamInfo, camera_stream_should_stop,
        mark_camera_stream_ended, preferred_v4l2_formats, store_latest_frame,
    },
    rgb_frame::{V4l2PixelFormat, decode_camera_frame, frame_len, mirror_rgb24_in_place},
};

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
const V4L2_FRMSIZE_TYPE_STEPWISE: u32 = 3;
const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;
const V4L2_FRMIVAL_TYPE_STEPWISE: u32 = 3;

const VIDIOC_ENUM_FMT: c_ulong = 0xc040_5602;
const VIDIOC_S_FMT: c_ulong = 0xc0d0_5605;
const VIDIOC_REQBUFS: c_ulong = 0xc014_5608;
const VIDIOC_QUERYBUF: c_ulong = 0xc058_5609;
const VIDIOC_QBUF: c_ulong = 0xc058_560f;
const VIDIOC_DQBUF: c_ulong = 0xc058_5611;
const VIDIOC_STREAMON: c_ulong = 0x8004_5612;
const VIDIOC_STREAMOFF: c_ulong = 0x8004_5613;
const VIDIOC_S_PARM: c_ulong = 0xc0cc_5616;
const VIDIOC_ENUM_FRAMESIZES: c_ulong = 0xc02c_564a;
const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong = 0xc034_564b;

unsafe extern "C" {
    #[link_name = "v4l2_open"]
    fn libv4l2_open(file: *const c_char, oflag: c_int, ...) -> c_int;
    #[link_name = "v4l2_close"]
    fn libv4l2_close(fd: c_int) -> c_int;
    #[link_name = "v4l2_ioctl"]
    fn libv4l2_ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    #[link_name = "v4l2_mmap"]
    fn libv4l2_mmap(
        start: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    #[link_name = "v4l2_munmap"]
    fn libv4l2_munmap(start: *mut c_void, length: usize) -> c_int;
}

#[link(name = "v4l2")]
unsafe extern "C" {}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
struct V4l2Fmtdesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
struct V4l2Format {
    type_: u32,
    union_align: u32,
    pix: V4l2PixFormat,
    padding: [u8; 152],
}

#[repr(C)]
struct V4l2StreamParmCapture {
    capability: u32,
    capturemode: u32,
    timeperframe: V4l2Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct V4l2StreamParm {
    type_: u32,
    capture: V4l2StreamParmCapture,
    padding: [u8; 160],
}

#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    m_offset: u32,
    m_padding: u32,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2FrmSizeDiscrete {
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2FrmSizeStepwise {
    min_width: u32,
    max_width: u32,
    step_width: u32,
    min_height: u32,
    max_height: u32,
    step_height: u32,
}

#[repr(C)]
union V4l2FrmSizeUnion {
    discrete: V4l2FrmSizeDiscrete,
    stepwise: V4l2FrmSizeStepwise,
}

#[repr(C)]
struct V4l2FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    size: V4l2FrmSizeUnion,
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2FrmIvalStepwise {
    min: V4l2Fract,
    max: V4l2Fract,
    step: V4l2Fract,
}

#[repr(C)]
union V4l2FrmIvalUnion {
    discrete: V4l2Fract,
    stepwise: V4l2FrmIvalStepwise,
}

#[repr(C)]
struct V4l2FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    interval: V4l2FrmIvalUnion,
    reserved: [u32; 2],
}

struct Device {
    fd: c_int,
}

impl Device {
    fn open(path: &str) -> io::Result<Self> {
        let path = CString::new(path).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "device path contains a NUL byte")
        })?;
        let fd = unsafe { libv4l2_open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self { fd })
        }
    }

    fn ioctl<T>(&self, request: c_ulong, arg: &mut T) -> io::Result<()> {
        ioctl(self.fd, request, arg)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = unsafe { libv4l2_close(self.fd) };
    }
}

struct MappedBuffer {
    ptr: *mut u8,
    len: usize,
}

struct MmapStream {
    fd: c_int,
    buffers: Vec<MappedBuffer>,
    streaming: bool,
}

impl MmapStream {
    fn new(device: &Device, requested_count: u32) -> io::Result<Self> {
        let mut req = V4l2RequestBuffers {
            count: requested_count,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: V4L2_MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
        };
        device.ioctl(VIDIOC_REQBUFS, &mut req)?;
        if req.count == 0 {
            return Err(io::Error::other("device did not allocate mmap buffers"));
        }

        let mut buffers = Vec::with_capacity(req.count as usize);
        for index in 0..req.count {
            let mut buf = v4l2_buffer(index);
            device.ioctl(VIDIOC_QUERYBUF, &mut buf)?;
            let mapped = unsafe {
                libv4l2_mmap(
                    ptr::null_mut(),
                    buf.length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    device.fd,
                    i64::from(buf.m_offset),
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            buffers.push(MappedBuffer {
                ptr: mapped.cast::<u8>(),
                len: buf.length as usize,
            });
            device.ioctl(VIDIOC_QBUF, &mut buf)?;
        }

        let mut stream = Self {
            fd: device.fd,
            buffers,
            streaming: false,
        };
        let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        device.ioctl(VIDIOC_STREAMON, &mut type_)?;
        stream.streaming = true;
        Ok(stream)
    }

    fn next_frame(&mut self, timeout: Duration) -> io::Result<(&[u8], usize)> {
        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(c_int::MAX as u128) as c_int;
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready == 0 {
            return Err(io::Error::new(ErrorKind::TimedOut, "v4l2 frame timed out"));
        }
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buf = v4l2_buffer(0);
        ioctl(self.fd, VIDIOC_DQBUF, &mut buf)?;
        let index = buf.index as usize;
        let Some(mapped) = self.buffers.get(index) else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("device dequeued invalid buffer index {}", buf.index),
            ));
        };
        let used = (buf.bytesused as usize).min(mapped.len);
        let frame = unsafe { std::slice::from_raw_parts(mapped.ptr, used) };
        Ok((frame, index))
    }

    fn queue_buffer(&self, index: usize) -> io::Result<()> {
        let mut buf = v4l2_buffer(index as u32);
        ioctl(self.fd, VIDIOC_QBUF, &mut buf)
    }
}

impl Drop for MmapStream {
    fn drop(&mut self) {
        if self.streaming {
            let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            let _ = ioctl(self.fd, VIDIOC_STREAMOFF, &mut type_);
        }
        for buffer in &self.buffers {
            let _ = unsafe { libv4l2_munmap(buffer.ptr.cast::<c_void>(), buffer.len) };
        }
    }
}

fn v4l2_buffer(index: u32) -> V4l2Buffer {
    V4l2Buffer {
        index,
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        bytesused: 0,
        flags: 0,
        field: 0,
        timestamp: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        timecode: V4l2Timecode::default(),
        sequence: 0,
        memory: V4L2_MEMORY_MMAP,
        m_offset: 0,
        m_padding: 0,
        length: 0,
        reserved2: 0,
        request_fd: 0,
    }
}

fn ioctl<T>(fd: c_int, request: c_ulong, arg: &mut T) -> io::Result<()> {
    let ret = unsafe { libv4l2_ioctl(fd, request, arg as *mut T) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn best_camera_mode(path: &str) -> Result<CameraMode> {
    let device = Device::open(path).context("failed to open v4l2 device")?;
    let mut modes = Vec::new();

    for fourcc in enum_formats(&device)? {
        let Some(pixel_format) = V4l2PixelFormat::from_fourcc(fourcc) else {
            continue;
        };
        for (width, height) in enum_frame_sizes(&device, fourcc)? {
            let fps = best_fps(&device, fourcc, width, height);
            modes.push(CameraMode {
                format: pixel_format.fourcc_name().to_string(),
                width,
                height,
                fps,
            });
        }
    }

    modes
        .into_iter()
        .max_by_key(camera_mode_preference)
        .ok_or_else(|| anyhow!("camera did not report usable capture modes"))
}

pub(super) fn run_capture(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    let device = Device::open(&config.device)
        .map_err(|error| format!("failed to open v4l2 device {}: {error}", config.device))?;

    let mut setup_errors = Vec::new();
    let Some((mut stream, frame_config, mut buffer)) = preferred_v4l2_formats(config)
        .into_iter()
        .find_map(|pixel_format| {
            let actual = match set_format(&device, config.width, config.height, pixel_format) {
                Ok(actual) => actual,
                Err(error) => {
                    setup_errors.push(format!("{pixel_format:?}: {error}"));
                    return None;
                }
            };
            let Some(actual_format) = V4l2PixelFormat::from_fourcc(actual.pixelformat) else {
                setup_errors.push(format!(
                    "{pixel_format:?}: device selected unsupported {}",
                    fourcc_name(actual.pixelformat)
                ));
                return None;
            };
            if let Err(error) = set_fps(&device, config.fps) {
                setup_errors.push(format!(
                    "{actual_format:?}: failed to set fps {}: {error}",
                    config.fps
                ));
                return None;
            }
            let frame_len = match frame_len(actual.width, actual.height) {
                Ok(frame_len) => frame_len,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: invalid frame size {}x{}: {error}",
                        actual.width, actual.height
                    ));
                    return None;
                }
            };
            let frame_config = V4l2FrameConfig {
                pixel_format: actual_format,
                width: actual.width,
                height: actual.height,
                mirror_horizontal: config.mirror_horizontal,
                frame_len,
            };
            let mut stream = match MmapStream::new(&device, 2) {
                Ok(stream) => stream,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to create mmap stream: {error}"
                    ));
                    return None;
                }
            };
            let mut buffer = vec![0_u8; frame_config.frame_len];
            match store_next_frame(
                &mut stream,
                frame_config,
                latest_frame,
                &mut buffer,
                Duration::from_secs(3),
            ) {
                Ok(reused) => Some((stream, frame_config, reused)),
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to read first frame: {error}"
                    ));
                    None
                }
            }
        })
    else {
        return Err(format!(
            "v4l2 device did not produce a frame for a supported RGB/YUYV/MJPG preview mode near {}x{}{}",
            config.width,
            config.height,
            if setup_errors.is_empty() {
                String::new()
            } else {
                format!(": {}", setup_errors.join("; "))
            }
        ));
    };

    let _ = ready.send(Ok(V4l2StreamInfo {
        width: frame_config.width,
        height: frame_config.height,
        input_format: frame_config.pixel_format.input_format(),
    }));

    loop {
        if camera_stream_should_stop(latest_frame) {
            break;
        }
        match store_next_frame(
            &mut stream,
            frame_config,
            latest_frame,
            &mut buffer,
            Duration::from_millis(100),
        ) {
            Ok(reused) => buffer = reused,
            Err(error) if error.kind() == ErrorKind::TimedOut => {}
            Err(error) => return Err(format!("failed to read v4l2 frame: {error}")),
        }
    }
    mark_camera_stream_ended(latest_frame);
    Ok(())
}

fn enum_formats(device: &Device) -> io::Result<Vec<u32>> {
    let mut formats = Vec::new();
    for index in 0.. {
        let mut desc = V4l2Fmtdesc {
            index,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            flags: 0,
            description: [0; 32],
            pixelformat: 0,
            mbus_code: 0,
            reserved: [0; 3],
        };
        match device.ioctl(VIDIOC_ENUM_FMT, &mut desc) {
            Ok(()) => formats.push(desc.pixelformat),
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(formats)
}

fn enum_frame_sizes(device: &Device, fourcc: u32) -> io::Result<Vec<(u32, u32)>> {
    let mut sizes = Vec::new();
    for index in 0.. {
        let mut frame_size = V4l2FrmSizeEnum {
            index,
            pixel_format: fourcc,
            type_: 0,
            size: V4l2FrmSizeUnion {
                discrete: V4l2FrmSizeDiscrete {
                    width: 0,
                    height: 0,
                },
            },
            reserved: [0; 2],
        };
        match device.ioctl(VIDIOC_ENUM_FRAMESIZES, &mut frame_size) {
            Ok(()) => match frame_size.type_ {
                V4L2_FRMSIZE_TYPE_DISCRETE => {
                    let discrete = unsafe { frame_size.size.discrete };
                    sizes.push((discrete.width, discrete.height));
                }
                V4L2_FRMSIZE_TYPE_STEPWISE => {
                    let stepwise = unsafe { frame_size.size.stepwise };
                    sizes.push((stepwise.max_width, stepwise.max_height));
                }
                _ => {}
            },
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(sizes)
}

fn best_fps(device: &Device, fourcc: u32, width: u32, height: u32) -> u32 {
    let mut best = None;
    for index in 0.. {
        let mut frame_interval = V4l2FrmIvalEnum {
            index,
            pixel_format: fourcc,
            width,
            height,
            type_: 0,
            interval: V4l2FrmIvalUnion {
                discrete: V4l2Fract {
                    numerator: 0,
                    denominator: 0,
                },
            },
            reserved: [0; 2],
        };
        match device.ioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut frame_interval) {
            Ok(()) => {
                let fps = match frame_interval.type_ {
                    V4L2_FRMIVAL_TYPE_DISCRETE => {
                        let discrete = unsafe { frame_interval.interval.discrete };
                        fps_from_interval(discrete.numerator, discrete.denominator)
                    }
                    V4L2_FRMIVAL_TYPE_STEPWISE => {
                        let stepwise = unsafe { frame_interval.interval.stepwise };
                        fps_from_interval(stepwise.min.numerator, stepwise.min.denominator)
                    }
                    _ => None,
                };
                best = best.max(fps);
            }
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
            Err(_) => break,
        }
    }
    best.unwrap_or(0)
}

fn set_format(
    device: &Device,
    width: u32,
    height: u32,
    pixel_format: V4l2PixelFormat,
) -> io::Result<V4l2PixFormat> {
    let mut format = V4l2Format {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        union_align: 0,
        pix: V4l2PixFormat {
            width,
            height,
            pixelformat: pixel_format.fourcc(),
            field: 0,
            bytesperline: 0,
            sizeimage: 0,
            colorspace: 0,
            priv_: 0,
            flags: 0,
            ycbcr_enc: 0,
            quantization: 0,
            xfer_func: 0,
        },
        padding: [0; 152],
    };
    device.ioctl(VIDIOC_S_FMT, &mut format)?;
    Ok(format.pix)
}

fn set_fps(device: &Device, fps: u32) -> io::Result<()> {
    if fps == 0 {
        return Ok(());
    }
    let mut params = V4l2StreamParm {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        capture: V4l2StreamParmCapture {
            capability: 0,
            capturemode: 0,
            timeperframe: V4l2Fract {
                numerator: 1,
                denominator: fps,
            },
            extendedmode: 0,
            readbuffers: 0,
            reserved: [0; 4],
        },
        padding: [0; 160],
    };
    device.ioctl(VIDIOC_S_PARM, &mut params)
}

fn store_next_frame(
    stream: &mut MmapStream,
    config: V4l2FrameConfig,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    buffer: &mut [u8],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let (frame, index) = stream.next_frame(timeout)?;
    let decode_result = decode_camera_frame(
        config.pixel_format,
        frame,
        config.width,
        config.height,
        buffer,
    )
    .map_err(io::Error::other);
    stream.queue_buffer(index)?;
    decode_result?;
    if config.mirror_horizontal {
        mirror_rgb24_in_place(buffer, config.width, config.height);
    }
    Ok(store_latest_frame(
        latest_frame,
        buffer.to_vec(),
        config.frame_len,
    ))
}

fn fourcc_name(fourcc: u32) -> String {
    let bytes = fourcc.to_le_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

const _: () = {
    assert!(mem::size_of::<V4l2Fmtdesc>() == 64);
    assert!(mem::size_of::<V4l2Format>() == 208);
    assert!(mem::size_of::<V4l2StreamParm>() == 204);
    assert!(mem::size_of::<V4l2RequestBuffers>() == 20);
    assert!(mem::size_of::<V4l2Buffer>() == 88);
    assert!(mem::size_of::<V4l2FrmSizeEnum>() == 44);
    assert!(mem::size_of::<V4l2FrmIvalEnum>() == 52);
};
