//! Video playback for Live Photos, decoded with FFmpeg.
//!
//! A background thread decodes the video and paces frames to their
//! timestamps; a [`VideoPlayer`] entity holds the latest one for rendering.
//! The sound, if wanted, starts with the first frame (see `audio`).
//!
//! - **macOS:** frames are hardware decoded (VideoToolbox) and stay on the
//!   GPU: GPUI draws them with its `surface` element.
//! - **Elsewhere:** frames are decoded in software and copied into regular
//!   GPUI images. GPUI's `surface` element is macOS-only for now; a
//!   cross-platform surface (D3D11 textures, DMA-BUF) would let Windows and
//!   Linux keep hardware frames on the GPU too.

mod audio;
#[cfg(target_os = "macos")]
mod macos;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling;
use ffmpeg_next::util::color;
use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use gpui_kit::{
    AnyElement, App, Context, ObjectFit, RenderImage, SharedString, StyledImage, Task, img,
    prelude::*,
};
use image::{DynamicImage, Frame, RgbaImage};

use crate::decode;

/// One decoded frame, ready to draw.
#[derive(Clone)]
enum VideoFrame {
    /// A hardware frame, drawn directly from the GPU.
    #[cfg(target_os = "macos")]
    Surface(objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>),
    /// A frame copied into a regular image.
    Image(Arc<RenderImage>),
}

impl VideoFrame {
    /// The frame's pixels, as shown.
    fn to_image(&self) -> Result<DynamicImage> {
        match self {
            #[cfg(target_os = "macos")]
            VideoFrame::Surface(buffer) => {
                Ok(DynamicImage::ImageRgb8(macos::surface_image(buffer)))
            }
            VideoFrame::Image(image) => {
                let size = image.size(0);
                let bytes = image
                    .as_bytes(0)
                    .ok_or_else(|| anyhow!("frame has no pixels"))?;
                // BGRA, as `FrameOutput::copy` left it.
                let mut buffer =
                    RgbaImage::from_raw(size.width.0 as u32, size.height.0 as u32, bytes.to_vec())
                        .ok_or_else(|| anyhow!("unexpected frame size"))?;
                for pixel in buffer.pixels_mut() {
                    pixel.0.swap(0, 2);
                }
                Ok(DynamicImage::ImageRgba8(buffer))
            }
        }
    }
}

/// Sent from the decode thread.
enum Message {
    Frame(VideoFrame),
    Finished,
    Failed(String),
}

// SAFETY: CVPixelBuffers are reference counted with atomic operations and
// immutable once decoded, so handing one to another thread is sound.
unsafe impl Send for Message {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Finished,
    Failed(SharedString),
}

/// Plays a video once, from the start.
pub struct VideoPlayer {
    frame: Option<VideoFrame>,
    state: PlaybackState,
    stop: Arc<AtomicBool>,
    _receive: Task<()>,
}

impl VideoPlayer {
    /// Plays `path`, with its sound if `with_sound`.
    pub fn new(path: PathBuf, with_sound: bool, cx: &mut Context<Self>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        // A small buffer: the decode thread paces frames itself.
        let (mut sender, mut receiver) = mpsc::channel(2);

        let thread_stop = stop.clone();
        let spawned = std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || {
                let sound = with_sound.then(|| load_sound(&path)).flatten();
                let result = decode(&path, &thread_stop, &mut sender, true, sound.as_ref());
                if let Some(sound) = &sound {
                    finish_sound(sound, &thread_stop);
                }
                let message = match result {
                    Ok(()) => Message::Finished,
                    Err(error) => Message::Failed(format!("{error:#}")),
                };
                futures::executor::block_on(sender.send(message)).ok();
            });
        let state = match spawned {
            Ok(_) => PlaybackState::Playing,
            Err(error) => PlaybackState::Failed(error.to_string().into()),
        };

        let receive = cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.receive(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            frame: None,
            state,
            stop,
            _receive: receive,
        }
    }

    pub fn state(&self) -> &PlaybackState {
        &self.state
    }

    /// The current frame, filling its parent.
    pub fn render_frame(&self, object_fit: ObjectFit) -> Option<AnyElement> {
        Some(match self.frame.as_ref()? {
            #[cfg(target_os = "macos")]
            // Positioned to fill the container exactly, rather than sized by
            // the frame's aspect ratio.
            VideoFrame::Surface(pixel_buffer) => gpui_kit::surface(pixel_buffer.clone())
                .absolute()
                .inset_0()
                .size_full()
                .object_fit(object_fit)
                .into_any_element(),
            VideoFrame::Image(image) => img(image.clone())
                .absolute()
                .inset_0()
                .size_full()
                .object_fit(object_fit)
                .into_any_element(),
        })
    }

    /// Stops decoding and frees the current frame.
    pub fn stop(&mut self, cx: &mut App) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(VideoFrame::Image(image)) = self.frame.take() {
            decode::release(image, cx);
        }
    }

    fn receive(&mut self, message: Message, cx: &mut Context<Self>) {
        match message {
            Message::Frame(frame) => {
                // Copied frames are new images each time; free the last one.
                if let Some(VideoFrame::Image(old)) = self.frame.replace(frame) {
                    decode::release(old, cx);
                }
            }
            Message::Finished => self.state = PlaybackState::Finished,
            Message::Failed(error) => self.state = PlaybackState::Failed(error.into()),
        }
        cx.notify();
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The video's sound, if it has any and there's something to play it on.
fn load_sound(path: &Path) -> Option<audio::Sound> {
    audio::Sound::load(path)
        .inspect_err(|error| eprintln!("couldn't load the Live Photo's sound: {error:#}"))
        .ok()
        .flatten()
}

/// The longest the sound plays on after the last frame.
const MAX_SOUND_TAIL: Duration = Duration::from_secs(2);

/// Lets the sound play to its end, unless playback's stopped.
fn finish_sound(sound: &audio::Sound, stop: &AtomicBool) {
    let give_up = Instant::now() + MAX_SOUND_TAIL;
    while !sound.is_finished() && !stop.load(Ordering::Relaxed) && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The video's first frame, as playing it would show it (so rotated, and
/// tone mapped where playing is).
pub fn first_frame(path: &Path) -> Result<DynamicImage> {
    let (mut sender, mut receiver) = mpsc::channel(2);
    std::thread::scope(|scope| {
        // Stops at the next frame once the receiver's gone.
        let decoding =
            scope.spawn(move || decode(path, &AtomicBool::new(false), &mut sender, true, None));
        let first = futures::executor::block_on(receiver.next());
        drop(receiver);
        let decoded = decoding
            .join()
            .map_err(|_| anyhow!("decoding {} panicked", path.display()))?;
        match first {
            Some(Message::Frame(frame)) => frame.to_image(),
            _ => decoded.and_then(|()| Err(anyhow!("no frames in {}", path.display()))),
        }
    })
}

/// Decodes the video's frames and sends them at their presentation times,
/// starting `sound` with the first. `hardware` enables hardware decoding
/// where supported.
fn decode(
    path: &Path,
    stop: &AtomicBool,
    sender: &mut mpsc::Sender<Message>,
    hardware: bool,
    sound: Option<&audio::Sound>,
) -> Result<()> {
    init_ffmpeg()?;
    let mut input =
        ffmpeg::format::input(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow!("no video stream"))?;
    let stream_index = stream.index();
    let time_base = f64::from(stream.time_base());
    let rotation = display_rotation(&stream);

    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    #[cfg(target_os = "macos")]
    if hardware {
        macos::enable_hardware_decoding(&mut context);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = hardware;
    let mut decoder = context.decoder().video()?;

    let mut output = FrameOutput::new(rotation);
    let mut clock: Option<(Instant, f64)> = None;
    let mut frame = ffmpeg::frame::Video::empty();
    let mut emit = |frame: &ffmpeg::frame::Video| -> Result<bool> {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let video_frame = output.convert(frame)?;

        // Wait until the frame is due.
        let time = frame.timestamp().map_or(0.0, |pts| pts as f64 * time_base);
        let (start, first_time) = *clock.get_or_insert_with(|| {
            if let Some(sound) = sound {
                sound.play();
            }
            (Instant::now(), time)
        });
        let due = start + Duration::from_secs_f64((time - first_time).max(0.0));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        Ok(futures::executor::block_on(sender.send(Message::Frame(video_frame))).is_ok())
    };

    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut frame).is_ok() {
            if !emit(&frame)? {
                return Ok(());
            }
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut frame).is_ok() {
        if !emit(&frame)? {
            return Ok(());
        }
    }
    Ok(())
}

fn init_ffmpeg() -> Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if ffmpeg::init().is_ok() {
            ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
        }
    });
    Ok(())
}

/// The container's metadata tags, e.g. QuickTime's location tag.
pub fn metadata(path: &Path) -> Result<Vec<(String, String)>> {
    init_ffmpeg()?;
    let input =
        ffmpeg::format::input(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(input
        .metadata()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect())
}

/// How far the video should be rotated clockwise for display (0, 90, 180
/// or 270 degrees), from the stream's display matrix. iPhones record
/// portrait video as landscape frames with a rotation.
fn display_rotation(stream: &ffmpeg::Stream) -> u32 {
    let counterclockwise = unsafe {
        let parameters = (*stream.as_ptr()).codecpar;
        let side_data = ffi::av_packet_side_data_get(
            (*parameters).coded_side_data,
            (*parameters).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if side_data.is_null() {
            return 0;
        }
        ffi::av_display_rotation_get((*side_data).data as *const i32)
    };
    clockwise_quarter_turns(counterclockwise)
}

/// Converts FFmpeg's counterclockwise display rotation to clockwise degrees,
/// snapped to a multiple of 90.
fn clockwise_quarter_turns(counterclockwise: f64) -> u32 {
    if !counterclockwise.is_finite() {
        return 0;
    }
    ((-counterclockwise / 90.).round() as i32 * 90).rem_euclid(360) as u32
}

/// Turns decoded frames into something GPUI can draw.
struct FrameOutput {
    rotation: u32,
    #[cfg(target_os = "macos")]
    surfaces: macos::SurfaceConverter,
    scaler: Option<(scaling::Context, ScalerKey)>,
}

/// What a scaler was set up for: the frame format, size and colour encoding.
type ScalerKey = (Pixel, u32, u32, color::Space, bool);

impl FrameOutput {
    fn new(rotation: u32) -> Self {
        Self {
            rotation,
            #[cfg(target_os = "macos")]
            surfaces: macos::SurfaceConverter::new(rotation),
            scaler: None,
        }
    }

    fn convert(&mut self, frame: &ffmpeg::frame::Video) -> Result<VideoFrame> {
        #[cfg(target_os = "macos")]
        if frame.format() == Pixel::VIDEOTOOLBOX {
            if let Some(pixel_buffer) = self.surfaces.convert(frame) {
                return Ok(VideoFrame::Surface(pixel_buffer));
            }
            // VideoToolbox couldn't convert the frame; copy it instead.
            let mut software = ffmpeg::frame::Video::empty();
            let status =
                unsafe { ffi::av_hwframe_transfer_data(software.as_mut_ptr(), frame.as_ptr(), 0) };
            if status < 0 {
                return Err(anyhow!("couldn't read back hardware frame ({status})"));
            }
            // Keep the colour tags.
            unsafe { ffi::av_frame_copy_props(software.as_mut_ptr(), frame.as_ptr()) };
            return self.copy(&software);
        }
        self.copy(frame)
    }

    /// Converts the frame to BGRA in memory and wraps it in an image. HDR
    /// isn't tone mapped on this path.
    fn copy(&mut self, frame: &ffmpeg::frame::Video) -> Result<VideoFrame> {
        let (width, height) = (frame.width(), frame.height());
        let full_range = frame.color_range() == color::Range::JPEG
            || matches!(
                frame.format(),
                Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P | Pixel::YUVJ440P
            );
        let key = (
            frame.format(),
            width,
            height,
            frame.color_space(),
            full_range,
        );
        if self.scaler.as_ref().is_none_or(|(_, k)| *k != key) {
            let mut scaler = scaling::Context::get(
                frame.format(),
                width,
                height,
                Pixel::BGRA,
                width,
                height,
                scaling::Flags::BILINEAR,
            )?;
            // swscale assumes BT.601 unless told the frame's encoding.
            // Untagged frames keep that default, as on the GPU path.
            unsafe {
                let source = ffi::AVColorSpace::from(frame.color_space()) as i32;
                ffi::sws_setColorspaceDetails(
                    scaler.as_mut_ptr(),
                    ffi::sws_getCoefficients(source),
                    full_range.into(),
                    ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }
            self.scaler = Some((scaler, key));
        }
        let (scaler, _) = self.scaler.as_mut().unwrap();
        let mut bgra = ffmpeg::frame::Video::empty();
        scaler.run(frame, &mut bgra)?;

        let row_bytes = width as usize * 4;
        let stride = bgra.stride(0);
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for row in bgra.data(0).chunks(stride).take(height as usize) {
            pixels.extend_from_slice(&row[..row_bytes]);
        }
        // GPUI images are BGRA, which is what the scaler produced; the
        // buffer type only names the channels.
        let buffer = RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(|| anyhow!("unexpected frame size"))?;
        let buffer = match self.rotation {
            90 => image::imageops::rotate90(&buffer),
            180 => image::imageops::rotate180(&buffer),
            270 => image::imageops::rotate270(&buffer),
            _ => buffer,
        };
        Ok(VideoFrame::Image(Arc::new(RenderImage::new([Frame::new(
            buffer,
        )]))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_display_rotation_to_clockwise_degrees() {
        // iPhone portrait video: FFmpeg reports -90 (i.e. 90° clockwise).
        assert_eq!(clockwise_quarter_turns(-90.), 90);
        assert_eq!(clockwise_quarter_turns(90.), 270);
        assert_eq!(clockwise_quarter_turns(180.), 180);
        assert_eq!(clockwise_quarter_turns(-0.4), 0);
        assert_eq!(clockwise_quarter_turns(f64::NAN), 0);
    }

    /// Frame sizes (width, height) and whether each stayed on the GPU.
    fn decode_all(path: &Path, hardware: bool) -> (Vec<(u32, u32, bool)>, bool) {
        let (mut sender, receiver) = mpsc::channel(2);
        let path = path.to_path_buf();
        let thread = std::thread::spawn(move || {
            let result = decode(&path, &AtomicBool::new(false), &mut sender, hardware, None);
            drop(sender);
            result
        });
        let messages: Vec<Message> = futures::executor::block_on(receiver.collect());
        let finished = thread.join().unwrap().is_ok();
        let frames = messages
            .into_iter()
            .filter_map(|message| match message {
                #[cfg(target_os = "macos")]
                Message::Frame(VideoFrame::Surface(buffer)) => {
                    use objc2_core_video::*;
                    assert_eq!(
                        CVPixelBufferGetPixelFormatType(&buffer),
                        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                        "GPUI's surface element only accepts this format"
                    );
                    Some((
                        CVPixelBufferGetWidth(&buffer) as u32,
                        CVPixelBufferGetHeight(&buffer) as u32,
                        true,
                    ))
                }
                Message::Frame(VideoFrame::Image(image)) => {
                    let size = image.size(0);
                    Some((size.width.0 as u32, size.height.0 as u32, false))
                }
                _ => None,
            })
            .collect();
        (frames, finished)
    }

    /// The first frame of a video and the colour shown at its centre.
    fn first_frame_colour(path: &Path, hardware: bool) -> (VideoFrame, [u8; 3]) {
        let (mut sender, receiver) = mpsc::channel(2);
        let path = path.to_path_buf();
        let thread = std::thread::spawn(move || {
            decode(&path, &AtomicBool::new(false), &mut sender, hardware, None).unwrap();
        });
        let mut messages = futures::executor::block_on_stream(receiver);
        let Some(Message::Frame(frame)) = messages.next() else {
            panic!("no frame");
        };
        drop(messages);
        thread.join().ok();
        let colour = match &frame {
            #[cfg(target_os = "macos")]
            VideoFrame::Surface(buffer) => macos::surface_centre_colour(buffer),
            VideoFrame::Image(image) => {
                let size = image.size(0);
                let (width, height) = (size.width.0 as usize, size.height.0 as usize);
                let offset = (height / 2 * width + width / 2) * 4;
                let bgra = &image.as_bytes(0).unwrap()[offset..offset + 4];
                [bgra[2], bgra[1], bgra[0]]
            }
        };
        (frame, colour)
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn assert_close(actual: [u8; 3], expected: [u8; 3], context: &str) {
        let close = actual
            .iter()
            .zip(expected)
            .all(|(&a, e)| a.abs_diff(e) <= 4);
        assert!(close, "{context}: got {actual:?}, expected {expected:?}");
    }

    /// The fixtures are a solid #C03060 encoded in different ways; it should
    /// look the same on screen whatever the encoding.
    #[test]
    fn shows_sdr_colours_correctly() {
        const EXPECTED: [u8; 3] = [0xC0, 0x30, 0x60];
        for name in ["bt709-limited.mov", "bt601-full.mov"] {
            for hardware in [true, false] {
                let (frame, colour) = first_frame_colour(&fixture(name), hardware);
                #[cfg(target_os = "macos")]
                assert_eq!(matches!(frame, VideoFrame::Surface(_)), hardware);
                #[cfg(not(target_os = "macos"))]
                let _ = frame;
                assert_close(colour, EXPECTED, &format!("{name} (hardware: {hardware})"));
            }
        }
    }

    /// Posters look as the video does, however it's encoded.
    #[test]
    fn first_frames_show_the_videos_colours() {
        for name in ["bt709-limited.mov", "bt601-full.mov"] {
            let image = first_frame(&fixture(name)).unwrap().into_rgb8();
            let centre = image.get_pixel(image.width() / 2, image.height() / 2).0;
            assert_close(centre, [0xC0, 0x30, 0x60], name);
        }
    }

    #[test]
    fn first_frame_of_something_else_fails() {
        assert!(first_frame(Path::new("Cargo.toml")).is_err());
    }

    /// Unlike the copy path, posters of HDR video aren't washed out.
    #[cfg(target_os = "macos")]
    #[test]
    fn first_frames_of_hdr_video_are_tone_mapped() {
        let image = first_frame(&fixture("hlg-bt2020.mov")).unwrap().into_rgb8();
        let [r, g, b] = image.get_pixel(image.width() / 2, image.height() / 2).0;
        assert!(r > 128 && r > g + 64 && r > b + 32, "got {:?}", [r, g, b]);
    }

    /// HDR frames are tone mapped to SDR in GPUI's surface format. The exact
    /// colour depends on the tone mapping, but it should stay recognisable.
    #[cfg(target_os = "macos")]
    #[test]
    fn tone_maps_hdr_video() {
        let (frame, [r, g, b]) = first_frame_colour(&fixture("hlg-bt2020.mov"), true);
        assert!(matches!(frame, VideoFrame::Surface(_)));
        eprintln!("HLG frame shown as {:?}", [r, g, b]);
        assert!(r > 128 && r > g + 64 && r > b + 32, "got {:?}", [r, g, b]);
    }

    /// Decodes a real video, e.g. a portrait iPhone Live Photo. Set
    /// `LIVE_PHOTO_VIDEO` to its path to run this.
    #[test]
    fn decodes_live_photo_video() {
        let Some(path) = std::env::var_os("LIVE_PHOTO_VIDEO") else {
            eprintln!("skipping: LIVE_PHOTO_VIDEO not set");
            return;
        };
        let path = PathBuf::from(path);

        let (frames, finished) = decode_all(&path, true);
        assert!(finished && !frames.is_empty());
        eprintln!("hardware: {} frames, first {:?}", frames.len(), frames[0]);
        #[cfg(target_os = "macos")]
        assert!(frames.iter().all(|&(_, _, on_gpu)| on_gpu));

        let (copied, finished) = decode_all(&path, false);
        assert!(finished);
        eprintln!("software: {} frames, first {:?}", copied.len(), copied[0]);
        assert!(copied.iter().all(|&(_, _, on_gpu)| !on_gpu));
        // Both paths agree on the frame count and upright size.
        assert_eq!(copied.len(), frames.len());
        assert_eq!((copied[0].0, copied[0].1), (frames[0].0, frames[0].1));
    }
}
