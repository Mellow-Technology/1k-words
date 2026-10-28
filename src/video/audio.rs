//! Sound for Live Photos: the video's audio track, decoded in full (it's
//! only a few seconds) and played through the default output device.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Sample;
use ffmpeg_next::format::sample::Type;
use ffmpeg_next::software::resampling;
use ffmpeg_next::{ChannelLayout, ffi};

/// A video's sound, ready to play once.
pub struct Sound {
    stream: cpal::Stream,
    /// Samples played so far, of `len`.
    played: Arc<AtomicUsize>,
    len: usize,
}

impl Sound {
    /// Decodes `path`'s audio for the default output device. None if the
    /// video has no sound, or there's nothing to play it on.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let Some(device) = cpal::default_host().default_output_device() else {
            return Ok(None);
        };
        let supported = device.default_output_config()?;
        let config = supported.config();
        let Some(samples) = decode(path, config.sample_rate, config.channels)? else {
            return Ok(None);
        };
        let (len, played) = (samples.len(), Arc::new(AtomicUsize::new(0)));
        let stream = match supported.sample_format() {
            SampleFormat::F32 => output::<f32>(&device, &config, samples, played.clone()),
            SampleFormat::I16 => output::<i16>(&device, &config, samples, played.clone()),
            SampleFormat::I32 => output::<i32>(&device, &config, samples, played.clone()),
            format => bail!("unsupported output format {format}"),
        }?;
        Ok(Some(Self {
            stream,
            played,
            len,
        }))
    }

    pub fn play(&self) {
        if let Err(error) = self.stream.play() {
            eprintln!("couldn't play the Live Photo's sound: {error}");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.played.load(Ordering::Relaxed) >= self.len
    }
}

/// An output stream, paused, that plays `samples` once, then silence.
fn output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    samples: Vec<f32>,
    played: Arc<AtomicUsize>,
) -> Result<cpal::Stream> {
    let stream = device.build_output_stream(
        config.clone(),
        move |out: &mut [T], _| {
            let start = played.load(Ordering::Relaxed).min(samples.len());
            let source = &samples[start..];
            for (i, out) in out.iter_mut().enumerate() {
                *out = T::from_sample(source.get(i).copied().unwrap_or(0.0));
            }
            played.store(start + out.len(), Ordering::Relaxed);
        },
        |error| eprintln!("Live Photo sound: {error}"),
        None,
    )?;
    stream.pause().ok();
    Ok(stream)
}

/// Decodes the video's audio to interleaved `f32` samples at `rate`, with
/// `channels` channels. None if it has no audio track.
pub fn decode(path: &Path, rate: u32, channels: u16) -> Result<Option<Vec<f32>>> {
    super::init_ffmpeg()?;
    let mut input =
        ffmpeg::format::input(path).with_context(|| format!("opening {}", path.display()))?;
    let Some(stream) = input.streams().best(ffmpeg::media::Type::Audio) else {
        return Ok(None);
    };
    let stream_index = stream.index();
    let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = context.decoder().audio()?;

    let output = Output {
        layout: ChannelLayout::default(channels.into()),
        rate,
        channels,
    };
    let mut resampler: Option<resampling::Context> = None;
    let mut samples = Vec::new();
    let mut frame = ffmpeg::frame::Audio::empty();
    let mut resample = |frame: &mut ffmpeg::frame::Audio, samples: &mut Vec<f32>| -> Result<()> {
        // Take an untagged channel layout (as in iPhones' PCM sound) as the
        // usual one for its channel count. The frame has to say so too, or
        // the resampler sees its input change.
        if frame.channel_layout().is_empty() {
            let count = frame.channel_layout().channels();
            unsafe { ffi::av_channel_layout_default(&mut (*frame.as_mut_ptr()).ch_layout, count) };
        }
        if resampler.is_none() {
            resampler = Some(resampling::Context::get(
                frame.format(),
                frame.channel_layout(),
                frame.rate(),
                Sample::F32(Type::Packed),
                output.layout.clone(),
                rate,
            )?);
        }
        output.convert(resampler.as_mut().unwrap(), Some(frame), samples)
    };

    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut frame).is_ok() {
            resample(&mut frame, &mut samples)?;
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut frame).is_ok() {
        resample(&mut frame, &mut samples)?;
    }
    // What the resampler held back.
    if let Some(resampler) = resampler.as_mut() {
        output.convert(resampler, None, &mut samples)?;
    }
    if samples.is_empty() {
        return Err(anyhow!("the audio track has no samples"));
    }
    Ok(Some(samples))
}

/// The format sound is resampled to: packed `f32`.
struct Output {
    layout: ChannelLayout,
    rate: u32,
    channels: u16,
}

impl Output {
    /// Resamples `input`, or with none, what the resampler's held back,
    /// adding the samples to `samples`. The output has room for all of
    /// them: when upsampling there are more than went in.
    fn convert(
        &self,
        resampler: &mut resampling::Context,
        input: Option<&ffmpeg::frame::Audio>,
        samples: &mut Vec<f32>,
    ) -> Result<()> {
        let in_samples = input.map_or(0, |frame| frame.samples() as i32);
        let capacity = unsafe { ffi::swr_get_out_samples(resampler.as_mut_ptr(), in_samples) };
        if capacity <= 0 {
            return Ok(());
        }
        let mut converted = ffmpeg::frame::Audio::new(
            Sample::F32(Type::Packed),
            capacity as usize,
            self.layout.clone(),
        );
        converted.set_rate(self.rate);
        let input = input.map_or(std::ptr::null(), |frame| unsafe { frame.as_ptr() });
        let status = unsafe {
            ffi::swr_convert_frame(resampler.as_mut_ptr(), converted.as_mut_ptr(), input)
        };
        if status < 0 {
            bail!("resampling failed: {}", ffmpeg::Error::from(status));
        }
        append(&converted, self.channels, samples);
        Ok(())
    }
}

/// Adds a frame of packed `f32` samples to `samples`.
fn append(frame: &ffmpeg::frame::Audio, channels: u16, samples: &mut Vec<f32>) {
    let count = frame.samples() * usize::from(channels);
    let bytes = &frame.data(0)[..count * 4];
    samples.extend(
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Root mean square of one channel of interleaved samples.
    fn rms(samples: &[f32], channels: usize, channel: usize) -> f32 {
        let values: Vec<f32> = samples
            .iter()
            .skip(channel)
            .step_by(channels)
            .copied()
            .collect();
        (values.iter().map(|v| v * v).sum::<f32>() / values.len() as f32).sqrt()
    }

    #[test]
    fn resamples_mono_sound_for_a_stereo_device() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/video/tone-aac.mov");
        let samples = decode(&path, 48_000, 2)
            .unwrap()
            .expect("the fixture has sound");
        // Half a second at 48 kHz, in stereo; AAC adds a little padding.
        let frames = samples.len() / 2;
        assert!((23_000..=26_000).contains(&frames), "{frames} frames");
        // A sine wave's RMS is about 0.7 of its peak; both channels carry it.
        let (left, right) = (rms(&samples, 2, 0), rms(&samples, 2, 1));
        assert!(left > 0.05, "left RMS {left}");
        assert!((left - right).abs() < 1e-3, "{left} vs {right}");
    }

    #[test]
    fn silent_videos_have_no_sound() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/video/bt601-full.mov");
        assert!(decode(&path, 48_000, 2).unwrap().is_none());
    }

    /// Decodes a real Live Photo's sound. Set `LIVE_PHOTO_VIDEO` to its path
    /// to run this.
    #[test]
    fn decodes_live_photo_sound() {
        let Some(path) = std::env::var_os("LIVE_PHOTO_VIDEO") else {
            eprintln!("skipping: LIVE_PHOTO_VIDEO not set");
            return;
        };
        let samples = decode(&PathBuf::from(path), 48_000, 2).unwrap().unwrap();
        let seconds = samples.len() as f32 / 2.0 / 48_000.0;
        eprintln!("{seconds:.2} s, RMS {}", rms(&samples, 2, 0));
        assert!((1.0..5.0).contains(&seconds));
    }
}
