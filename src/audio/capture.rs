//! State-of-the-art lock-free microphone capture using upstream `cpal 0.18`,
//! wait-free SPSC `rtrb 0.4`, and `rubato 5.1` FFT resampling to 16 kHz mono.

use crate::audio::visualizer::AudioVisualiser;
use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
const RESAMPLER_CHUNK_FRAMES: usize = 512;
const VISUALIZER_WINDOW: usize = 512;
const VISUALIZER_BUCKETS: usize = 16;
const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(33); // ~30 Hz

pub type AudioCallback = Arc<dyn Fn(&[f32]) + Send + Sync + 'static>;
pub type LevelCallback = Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>;
pub type ReadyCallback = Arc<dyn Fn(Duration) + Send + Sync + 'static>;

/// Enumerates currently connected macOS audio input devices by display name.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn select_input_device(preferred: Option<&str>) -> Result<(Device, String)> {
    let host = cpal::default_host();
    if let Some(wanted) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if let Ok(desc) = device.description()
                    && desc.name() == wanted
                {
                    return Ok((device, desc.name().to_string()));
                }
            }
        }
        tracing::warn!(
            preferred = wanted,
            "Selected microphone not found; falling back to default"
        );
    }

    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("No input microphone available"))?;
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "Default Microphone".to_string());
    Ok((device, name))
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureSummary {
    pub samples_16k: usize,
    pub input_silent: bool,
    pub mic_ready_ms: Option<f64>,
}

/// Active microphone recording session. Dropping or calling [`stop`] cleanly tears down
/// the CoreAudio stream and joins the consumer thread after draining remaining frames.
pub struct ActiveCapture {
    pub device_name: String,
    stream: Option<Stream>,
    stop_flag: Arc<AtomicBool>,
    consumer_thread: Option<JoinHandle<CaptureSummary>>,
}

impl ActiveCapture {
    pub fn start(
        preferred_mic: Option<&str>,
        started_at: Instant,
        on_audio: AudioCallback,
        on_levels: LevelCallback,
        on_ready: ReadyCallback,
    ) -> Result<Self> {
        let (device, device_name) = select_input_device(preferred_mic)?;
        let supported = device
            .default_input_config()
            .map_err(|e| anyhow!("Failed to query microphone config: {e}"))?;
        let sample_format = supported.sample_format();
        let stream_config: StreamConfig = supported.into();
        let input_rate = stream_config.sample_rate;
        let channels = stream_config.channels.max(1) as usize;

        tracing::info!(
            device = %device_name,
            input_rate,
            channels,
            format = ?sample_format,
            "Starting lock-free cpal audio capture"
        );

        // Pre-allocate ~2 seconds of mono float ring buffer so the CoreAudio RT callback
        // never allocates or blocks.
        let ring_capacity = (input_rate as usize * 2).max(16_384);
        let (producer, consumer) = RingBuffer::<f32>::new(ring_capacity);

        let stream_failed = Arc::new(AtomicBool::new(false));
        let stream_failed_cb = Arc::clone(&stream_failed);
        let err_fn = move |err| {
            tracing::error!(error = %err, "CoreAudio input stream error");
            stream_failed_cb.store(true, Ordering::Release);
        };

        let stream = match sample_format {
            SampleFormat::F32 => {
                let mut prod = producer;
                device.build_input_stream(
                    stream_config,
                    move |data: &[f32], _| push_mono_f32(&mut prod, data, channels),
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut prod = producer;
                device.build_input_stream(
                    stream_config,
                    move |data: &[i16], _| push_mono_i16(&mut prod, data, channels),
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut prod = producer;
                device.build_input_stream(
                    stream_config,
                    move |data: &[u16], _| push_mono_u16(&mut prod, data, channels),
                    err_fn,
                    None,
                )
            }
            other => return Err(anyhow!("Unsupported microphone sample format: {other:?}")),
        }
        .map_err(|e| anyhow!("Failed to open microphone input stream: {e}"))?;

        stream
            .play()
            .map_err(|e| anyhow!("Failed to start microphone stream: {e}"))?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);
        let consumer_thread = thread::Builder::new()
            .name("vibo-audio-consumer".into())
            .spawn(move || {
                run_consumer(
                    consumer, input_rate, started_at, stop_clone, on_audio, on_levels, on_ready,
                )
            })
            .map_err(|e| anyhow!("Failed to spawn audio consumer thread: {e}"))?;

        Ok(Self {
            device_name,
            stream: Some(stream),
            stop_flag,
            consumer_thread: Some(consumer_thread),
        })
    }

    pub fn stop(mut self) -> CaptureSummary {
        // Drop CoreAudio stream first so no new frames arrive, then signal consumer to drain.
        drop(self.stream.take());
        self.stop_flag.store(true, Ordering::Release);
        self.consumer_thread
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or(CaptureSummary {
                samples_16k: 0,
                input_silent: false,
                mic_ready_ms: None,
            })
    }
}

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        drop(self.stream.take());
        self.stop_flag.store(true, Ordering::Release);
        if let Some(h) = self.consumer_thread.take() {
            let _ = h.join();
        }
    }
}

/// Zero-allocation mono downmix pushed straight into the wait-free ring buffer.
fn push_mono_f32(prod: &mut Producer<f32>, data: &[f32], channels: usize) {
    if channels == 1 {
        if let Ok(chunk) = prod.write_chunk_uninit(data.len().min(prod.slots())) {
            let n = chunk.len();
            chunk.fill_from_iter(data[..n].iter().copied());
        }
        return;
    }
    let inv = 1.0 / channels as f32;
    let frames = (data.len() / channels).min(prod.slots());
    if frames == 0 {
        return;
    }
    if let Ok(chunk) = prod.write_chunk_uninit(frames) {
        chunk.fill_from_iter(
            data.chunks_exact(channels)
                .take(frames)
                .map(|frame| frame.iter().sum::<f32>() * inv),
        );
    }
}

fn push_mono_i16(prod: &mut Producer<f32>, data: &[i16], channels: usize) {
    let scale = 1.0 / (i16::MAX as f32 * channels as f32);
    let frames = (data.len() / channels).min(prod.slots());
    if frames == 0 {
        return;
    }
    if let Ok(chunk) = prod.write_chunk_uninit(frames) {
        chunk.fill_from_iter(data.chunks_exact(channels).take(frames).map(|frame| {
            let sum: f32 = frame.iter().map(|&s| s as f32).sum();
            sum * scale
        }));
    }
}

fn push_mono_u16(prod: &mut Producer<f32>, data: &[u16], channels: usize) {
    let inv_ch = 1.0 / channels as f32;
    let frames = (data.len() / channels).min(prod.slots());
    if frames == 0 {
        return;
    }
    if let Ok(chunk) = prod.write_chunk_uninit(frames) {
        chunk.fill_from_iter(data.chunks_exact(channels).take(frames).map(|frame| {
            let sum: f32 = frame.iter().map(|&s| (s as f32 / 32768.0) - 1.0).sum();
            sum * inv_ch
        }));
    }
}

struct StreamingResampler {
    stage: Option<ResampleStage>,
}

struct ResampleStage {
    resampler: Fft<f32>,
    pending: Vec<f32>,
    scratch: Vec<f32>,
    delay_left: usize,
}

impl StreamingResampler {
    fn new(input_rate: u32) -> Result<Self> {
        if input_rate == TARGET_SAMPLE_RATE {
            return Ok(Self { stage: None });
        }
        let resampler = Fft::<f32>::new(
            input_rate as usize,
            TARGET_SAMPLE_RATE as usize,
            RESAMPLER_CHUNK_FRAMES,
            1,
            FixedSync::Input,
        )
        .map_err(|e| anyhow!("Failed to initialize rubato resampler ({input_rate} Hz): {e}"))?;
        let pending = Vec::with_capacity(resampler.input_frames_max() * 2);
        let scratch = vec![0.0_f32; resampler.output_frames_max()];
        let delay_left = resampler.output_delay();
        Ok(Self {
            stage: Some(ResampleStage {
                resampler,
                pending,
                scratch,
                delay_left,
            }),
        })
    }

    fn process(&mut self, incoming: &[f32], out: &mut Vec<f32>) {
        out.clear();
        let Some(stage) = self.stage.as_mut() else {
            out.extend_from_slice(incoming);
            return;
        };
        stage.pending.extend_from_slice(incoming);
        let mut offset = 0;
        let step_in = stage.resampler.input_frames_next();
        while stage.pending.len() - offset >= step_in {
            if let Ok(taken) = stage.step(offset, None, out) {
                offset += taken;
            } else {
                break;
            }
        }
        if offset > 0 {
            stage.pending.drain(..offset);
        }
    }

    fn flush(&mut self, out: &mut Vec<f32>) {
        out.clear();
        let Some(stage) = self.stage.as_mut() else {
            return;
        };
        if !stage.pending.is_empty() {
            let left = stage.pending.len();
            let _ = stage.step(0, Some(left), out);
            stage.pending.clear();
        }
    }
}

impl ResampleStage {
    fn step(
        &mut self,
        offset: usize,
        partial: Option<usize>,
        output: &mut Vec<f32>,
    ) -> Result<usize> {
        let input = InterleavedSlice::new(&self.pending[..], 1, self.pending.len())
            .map_err(|e| anyhow!("Resampler input adapter error: {e}"))?;
        let scratch_len = self.scratch.len();
        let mut out_slice = InterleavedSlice::new_mut(&mut self.scratch[..], 1, scratch_len)
            .map_err(|e| anyhow!("Resampler output adapter error: {e}"))?;
        let mut indexing = Indexing::new().input_offset(offset);
        indexing.partial_len = partial;
        let (taken, produced) = self
            .resampler
            .process_into_buffer(&input, &mut out_slice, Some(&indexing))
            .map_err(|e| anyhow!("Resampler process error: {e}"))?;
        let dropped = self.delay_left.min(produced);
        self.delay_left -= dropped;
        output.extend_from_slice(&self.scratch[dropped..produced]);
        Ok(taken)
    }
}

fn run_consumer(
    mut consumer: Consumer<f32>,
    input_rate: u32,
    started_at: Instant,
    stop_flag: Arc<AtomicBool>,
    on_audio: AudioCallback,
    on_levels: LevelCallback,
    on_ready: ReadyCallback,
) -> CaptureSummary {
    let mut resampler = match StreamingResampler::new(input_rate) {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(error = %err, "Resampler initialization failed");
            return CaptureSummary {
                samples_16k: 0,
                input_silent: false,
                mic_ready_ms: None,
            };
        }
    };

    let mut visualizer = AudioVisualiser::new(
        TARGET_SAMPLE_RATE,
        VISUALIZER_WINDOW,
        VISUALIZER_BUCKETS,
        400.0,
        4000.0,
    );
    let mut raw_buf = Vec::with_capacity(2048);
    let mut out_16k = Vec::with_capacity(2048);
    let mut saw_any_audio = false;
    let mut heard_non_zero = false;
    let mut mic_ready_ms = None;
    let mut samples_16k = 0usize;
    let mut last_level_emit = Instant::now() - LEVEL_EMIT_INTERVAL;

    loop {
        raw_buf.clear();
        let avail = consumer.slots();
        if avail > 0
            && let Ok(chunk) = consumer.read_chunk(avail)
        {
            let (a, b) = chunk.as_slices();
            raw_buf.extend_from_slice(a);
            raw_buf.extend_from_slice(b);
            chunk.commit_all();
        }

        if !raw_buf.is_empty() {
            if !saw_any_audio {
                saw_any_audio = true;
                let elapsed = started_at.elapsed();
                mic_ready_ms = Some(crate::telemetry::ms(elapsed));
                on_ready(elapsed);
            }
            if !heard_non_zero && raw_buf.iter().any(|&s| s.abs() > 1e-7) {
                heard_non_zero = true;
            }

            resampler.process(&raw_buf, &mut out_16k);
            if !out_16k.is_empty() {
                samples_16k += out_16k.len();
                on_audio(&out_16k);
                if let Some(levels) = visualizer.feed(&out_16k)
                    && last_level_emit.elapsed() >= LEVEL_EMIT_INTERVAL
                {
                    last_level_emit = Instant::now();
                    on_levels(levels);
                }
            }
        } else if stop_flag.load(Ordering::Acquire) {
            break;
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Flush any tail frames held by the resampler.
    resampler.flush(&mut out_16k);
    if !out_16k.is_empty() {
        samples_16k += out_16k.len();
        on_audio(&out_16k);
    }

    CaptureSummary {
        samples_16k,
        input_silent: saw_any_audio && !heard_non_zero,
        mic_ready_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48khz_and_44_1khz_to_expected_16khz_lengths() {
        for input_rate in [48_000_u32, 44_100_u32, 24_000_u32, 16_000_u32] {
            let mut r = StreamingResampler::new(input_rate).unwrap();
            let input: Vec<f32> = (0..input_rate as usize)
                .map(|i| {
                    (2.0 * std::f32::consts::PI * 440.0 * i as f32 / input_rate as f32).sin() * 0.5
                })
                .collect();
            let mut out = Vec::new();
            let mut total = Vec::new();
            for chunk in input.chunks(480) {
                r.process(chunk, &mut out);
                total.extend_from_slice(&out);
            }
            r.flush(&mut out);
            total.extend_from_slice(&out);

            // 1 second of audio should land very close to 16,000 samples (minus small FFT delay tail).
            assert!(
                (total.len() as isize - 16_000).abs() < 600,
                "rate {input_rate} produced {} samples",
                total.len()
            );
        }
    }

    #[test]
    fn downmixes_stereo_f32_into_ring_buffer() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(64);
        push_mono_f32(&mut prod, &[0.2, 0.6, -0.4, 0.4], 2);
        let chunk = cons.read_chunk(2).unwrap();
        let (a, _) = chunk.as_slices();
        assert!((a[0] - 0.4).abs() < 1e-6);
        assert!(a[1].abs() < 1e-6);
    }
}
