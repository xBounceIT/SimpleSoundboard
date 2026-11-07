use std::{
    collections::VecDeque,
    fs::File,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use symphonia::core::{
    codecs::audio::AudioDecoderOptions,
    errors::Error as SymphoniaError,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

use crate::model::{MAX_SOUND_VOLUME, SoundId, normalize_playback_speed};

const WAVEFORM_BUCKETS: usize = 48;
const MIC_PREBUFFER_SECONDS: f32 = 0.03;
const MIC_BUFFER_CAP: usize = 8192;
const MONITOR_PREBUFFER_SECONDS: f32 = 0.04;
const MONITOR_MAX_SECONDS: f32 = 0.25;

#[derive(Clone, Debug)]
pub struct DecodedSound {
    pub samples: Arc<[f32]>,
    pub sample_rate: u32,
    pub duration: f32,
    pub waveform: Arc<[f32]>,
}

impl DecodedSound {
    fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Return a copy of this sound restricted to the given time range.
    ///
    /// The source remains untouched so the UI can offer a new trim later.
    pub fn trimmed(&self, start_seconds: f32, end_seconds: f32) -> DecodedSound {
        let total_frames = self.frames();
        if total_frames == 0 {
            return self.clone();
        }
        let start = start_seconds.clamp(0.0, self.duration);
        let mut end = end_seconds.clamp(start, self.duration);
        if end <= start {
            end = (start + 0.001).min(self.duration);
        }
        let start_frame =
            ((start * self.sample_rate as f32).round() as usize).min(total_frames - 1);
        let end_frame = ((end * self.sample_rate as f32).round() as usize)
            .max(start_frame + 1)
            .min(total_frames);
        if start_frame == 0 && end_frame == total_frames {
            return self.clone();
        }
        let samples = &self.samples[start_frame * 2..end_frame * 2];
        DecodedSound {
            samples: Arc::from(samples.to_vec().into_boxed_slice()),
            sample_rate: self.sample_rate,
            duration: (end_frame - start_frame) as f32 / self.sample_rate as f32,
            waveform: Arc::from(build_waveform(samples).into_boxed_slice()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceChoice {
    pub name: String,
    pub is_default: bool,
    pub is_virtual: bool,
}

impl DeviceChoice {
    pub fn label(&self) -> String {
        if self.is_virtual {
            format!("{}  ·  virtual", self.name)
        } else if self.is_default {
            format!("{}  ·  default", self.name)
        } else {
            self.name.clone()
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransportSnapshot {
    pub sound_id: SoundId,
    pub position: f32,
    pub duration: f32,
    /// Playback rate of the voice currently driving the transport.
    pub speed: f32,
}

impl Default for TransportSnapshot {
    fn default() -> Self {
        Self {
            sound_id: 0,
            position: 0.0,
            duration: 0.0,
            speed: 1.0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioSnapshot {
    pub output_level_l: f32,
    pub output_level_r: f32,
    pub mic_level: f32,
    /// Sounds with a voice on air, oldest first. More than one entry only in
    /// polyphonic mode.
    pub playing: Vec<SoundId>,
    pub current: Option<TransportSnapshot>,
    pub paused: bool,
}

struct Voice {
    token: u64,
    sound_id: SoundId,
    decoded: Arc<DecodedSound>,
    position: f64,
    gain: f32,
    /// Playback rate for this pad only.
    speed: f32,
    looping: bool,
    finished: bool,
}

struct CurrentPlayback {
    token: u64,
    sound_id: SoundId,
    duration: f32,
    speed: f32,
    looping: bool,
}

#[derive(Default)]
struct MicBuffer {
    samples: VecDeque<f32>,
    input_rate: u32,
    output_rate: u32,
    phase: f32,
    primed: bool,
}

impl MicBuffer {
    fn push_input(&mut self, frames: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        for frame in frames.chunks(channels) {
            let mono = if channels == 1 {
                frame[0]
            } else {
                (frame[0] + frame[1]) * 0.5
            };
            if self.samples.len() >= MIC_BUFFER_CAP {
                self.samples.pop_front();
                self.primed = false;
            }
            self.samples.push_back(mono.clamp(-1.0, 1.0));
        }
    }

    fn reset(&mut self) {
        self.samples.clear();
        self.phase = 0.0;
        self.primed = false;
    }

    fn next_sample(&mut self) -> f32 {
        if self.input_rate == 0 || self.output_rate == 0 || self.samples.len() < 2 {
            self.primed = false;
            return 0.0;
        }

        if !self.primed {
            let target = (self.input_rate as f32 * MIC_PREBUFFER_SECONDS) as usize;
            if self.samples.len() < target.max(2) {
                return 0.0;
            }
            self.primed = true;
            self.phase = 0.0;
        }

        let first = self.samples[0];
        let second = self.samples[1];
        let sample = first + (second - first) * self.phase;
        self.phase += self.input_rate as f32 / self.output_rate as f32;
        while self.phase >= 1.0 && self.samples.len() >= 2 {
            self.samples.pop_front();
            self.phase -= 1.0;
        }
        sample
    }
}

#[derive(Default)]
struct MonitorRing {
    frames: VecDeque<[f32; 2]>,
    source_rate: u32,
    phase: f32,
    primed: bool,
}

impl MonitorRing {
    fn reset(&mut self) {
        self.frames.clear();
        self.phase = 0.0;
        self.primed = false;
    }

    fn push_block(&mut self, interleaved: &[f32], source_rate: u32) {
        if source_rate == 0 {
            return;
        }
        if self.source_rate != source_rate {
            self.reset();
            self.source_rate = source_rate;
        }
        let max_frames = (source_rate as f32 * MONITOR_MAX_SECONDS) as usize;
        for frame in interleaved.chunks_exact(2) {
            if self.frames.len() >= max_frames.max(2) {
                self.frames.pop_front();
            }
            self.frames.push_back([frame[0], frame[1]]);
        }
    }

    fn fill_stereo(&mut self, output: &mut [f32], target_rate: u32) {
        if target_rate == 0 {
            output.fill(0.0);
            return;
        }
        if self.source_rate == 0 || self.frames.len() < 2 {
            output.fill(0.0);
            self.primed = false;
            return;
        }

        let prebuffer = (self.source_rate as f32 * MONITOR_PREBUFFER_SECONDS).max(2.0) as usize;
        if !self.primed {
            if self.frames.len() < prebuffer {
                output.fill(0.0);
                return;
            }
            self.primed = true;
            self.phase = 0.0;
        }

        let step = self.source_rate as f32 / target_rate as f32;
        for frame in output.chunks_exact_mut(2) {
            if self.frames.len() < 2 {
                frame[0] = 0.0;
                frame[1] = 0.0;
                self.primed = false;
                continue;
            }
            let a = self.frames[0];
            let b = self.frames[1];
            let t = self.phase.clamp(0.0, 1.0);
            frame[0] = a[0] + (b[0] - a[0]) * t;
            frame[1] = a[1] + (b[1] - a[1]) * t;
            self.phase += step;
            while self.phase >= 1.0 && self.frames.len() >= 2 {
                self.frames.pop_front();
                self.phase -= 1.0;
            }
        }
    }
}

/// Write one interleaved output frame from a stereo pair.
fn write_stereo(frame: &mut [f32], left: f32, right: f32) {
    if frame.len() == 1 {
        frame[0] = (left + right) * 0.5;
    } else {
        frame[0] = left;
        frame[1] = right;
        for sample in &mut frame[2..] {
            *sample = 0.0;
        }
    }
}

struct Mixer {
    voices: Vec<Voice>,
    current: Option<CurrentPlayback>,
    next_token: u64,
    master_volume: f32,
    mic_monitor: bool,
    mic_gain: f32,
    /// Keep the previous voices alive when a new pad is triggered.
    polyphonic: bool,
    mic: MicBuffer,
    paused: bool,
    output_rate: u32,
    output_channels: u16,
    level_l: f32,
    level_r: f32,
    mic_level: f32,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            voices: Vec::new(),
            current: None,
            next_token: 1,
            master_volume: 0.8,
            mic_monitor: false,
            mic_gain: 1.0,
            polyphonic: false,
            mic: MicBuffer::default(),
            paused: false,
            output_rate: 48_000,
            output_channels: 2,
            level_l: 0.0,
            level_r: 0.0,
            mic_level: 0.0,
        }
    }
}

impl Mixer {
    fn set_output_format(&mut self, rate: u32, channels: u16) {
        if self.output_rate != rate {
            self.mic.primed = false;
            self.mic.phase = 0.0;
        }
        self.output_rate = rate;
        self.output_channels = channels;
        self.mic.output_rate = rate;
    }

    fn trigger(
        &mut self,
        sound_id: SoundId,
        decoded: Arc<DecodedSound>,
        gain: f32,
        start_ratio: f32,
        speed: f32,
    ) {
        if self.polyphonic {
            // Pads stack: only the pad that was triggered again restarts, so
            // two different sounds can stay on air together.
            self.voices.retain(|voice| voice.sound_id != sound_id);
        } else {
            // A soundboard cue is exclusive: starting a clip releases the
            // previous one, so rapid clicks can never stack two cues together.
            self.voices.clear();
        }
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        let frames = decoded.frames() as f64;
        let position =
            (frames * f64::from(start_ratio.clamp(0.0, 1.0))).min((frames - 1.0).max(0.0));
        let speed = normalize_playback_speed(speed);
        self.voices.push(Voice {
            token,
            sound_id,
            decoded: decoded.clone(),
            position,
            gain: gain.clamp(0.0, MAX_SOUND_VOLUME),
            speed,
            looping: false,
            finished: false,
        });
        self.current = Some(CurrentPlayback {
            token,
            sound_id,
            duration: decoded.duration,
            speed,
            looping: false,
        });
        self.paused = false;
    }

    fn set_current_looping(&mut self, looping: bool) {
        if let Some(current) = &mut self.current {
            current.looping = looping;
            if let Some(voice) = self.voices.iter_mut().find(|v| v.token == current.token) {
                voice.looping = looping;
            }
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if self.current.is_some() {
            self.paused = paused;
        }
    }

    fn set_sound_volume(&mut self, sound_id: SoundId, volume: f32) {
        let gain = volume.clamp(0.0, MAX_SOUND_VOLUME);
        for voice in &mut self.voices {
            if voice.sound_id == sound_id {
                voice.gain = gain;
            }
        }
    }

    fn set_playback_speed(&mut self, sound_id: SoundId, speed: f32) {
        let speed = normalize_playback_speed(speed);
        for voice in &mut self.voices {
            if voice.sound_id == sound_id {
                voice.speed = speed;
            }
        }
        if let Some(current) = &mut self.current
            && current.sound_id == sound_id
        {
            current.speed = speed;
        }
    }

    fn stop_all(&mut self) {
        self.voices.clear();
        self.current = None;
        self.paused = false;
        self.level_l = 0.0;
        self.level_r = 0.0;
    }

    fn stop_sound(&mut self, sound_id: SoundId) {
        self.voices.retain(|voice| voice.sound_id != sound_id);
        self.refresh_current();
    }

    /// Keep the transport on a live voice. When the current one ends, the
    /// newest surviving voice takes over; with no voices left the transport
    /// goes idle.
    fn refresh_current(&mut self) {
        let Some(current) = &self.current else {
            return;
        };
        if self
            .voices
            .iter()
            .any(|voice| voice.token == current.token && !voice.finished)
        {
            return;
        }
        let next = self
            .voices
            .iter()
            .filter(|voice| !voice.finished)
            .max_by_key(|voice| voice.token)
            .map(|voice| CurrentPlayback {
                token: voice.token,
                sound_id: voice.sound_id,
                duration: voice.decoded.duration,
                speed: voice.speed,
                looping: voice.looping,
            });
        self.paused = false;
        self.current = next;
    }

    fn seek_to(&mut self, ratio: f32) {
        let Some(current) = &self.current else {
            return;
        };
        if let Some(voice) = self.voices.iter_mut().find(|v| v.token == current.token) {
            let frames = voice.decoded.frames() as f64;
            voice.position = frames * f64::from(ratio.clamp(0.0, 1.0));
            if voice.position >= frames {
                voice.position = (frames - 1.0).max(0.0);
            }
            voice.finished = false;
        }
    }

    fn push_mic(&mut self, samples: &[f32], channels: usize) {
        let mut peak = 0.0f32;
        let stride = channels.max(1);
        for frame in samples.chunks(stride) {
            let mono = if stride == 1 {
                frame[0]
            } else {
                (frame[0] + frame[1]) * 0.5
            };
            peak = peak.max(mono.abs());
        }
        self.mic_level = self.mic_level.max(peak).min(2.0).max(self.mic_level * 0.86);
        self.mic.push_input(samples, stride);
    }

    fn render(&mut self, output: &mut [f32], channels: usize, sample_rate: u32) {
        self.render_with_voice_bus(output, None, channels, sample_rate);
    }

    /// Render the full program (sounds + voice) into `output`.
    ///
    /// When `voice_bus` is given, the microphone-only mix is written there in
    /// the same pass, so the transport still advances exactly once. The
    /// headphones-only route sends `voice_bus` to the virtual microphone and
    /// keeps `output` — sounds and all — for the headphones.
    fn render_with_voice_bus(
        &mut self,
        output: &mut [f32],
        mut voice_bus: Option<&mut [f32]>,
        channels: usize,
        sample_rate: u32,
    ) {
        let channels = channels.max(1);
        let frames = output.len() / channels;
        if frames == 0 {
            return;
        }

        if self.output_rate != sample_rate {
            self.set_output_format(sample_rate, channels as u16);
        }

        if self.paused {
            output.fill(0.0);
            if let Some(voice) = voice_bus.as_deref_mut() {
                voice.fill(0.0);
            }
            self.level_l *= 0.88;
            self.level_r *= 0.88;
            return;
        }

        let master = self.master_volume;
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        let mic_enabled = self.mic_monitor && self.mic_gain > 0.001;
        let mic_gain = self.mic_gain;

        for frame_index in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;

            for voice in &mut self.voices {
                if voice.finished {
                    continue;
                }
                let total = voice.decoded.frames();
                if total == 0 {
                    voice.finished = true;
                    continue;
                }
                let pos = voice.position.floor() as usize;
                let frac = (voice.position - pos as f64) as f32;
                let index = pos.min(total - 1);
                let next = if index + 1 < total {
                    index + 1
                } else if voice.looping {
                    0
                } else {
                    index
                };
                let samples = &voice.decoded.samples;
                let l0 = samples[index * 2];
                let r0 = samples[index * 2 + 1];
                let l1 = samples[next * 2];
                let r1 = samples[next * 2 + 1];
                left += (l0 + (l1 - l0) * frac) * voice.gain;
                right += (r0 + (r1 - r0) * frac) * voice.gain;

                voice.position +=
                    f64::from(voice.speed) * voice.decoded.sample_rate as f64 / sample_rate as f64;
                if voice.position >= total as f64 {
                    if voice.looping {
                        voice.position %= total as f64;
                    } else {
                        voice.finished = true;
                    }
                }
            }

            let mut mic_sample = 0.0f32;
            if mic_enabled {
                mic_sample = self.mic.next_sample() * mic_gain;
                left += mic_sample;
                right += mic_sample;
            }

            let l = (left * master).clamp(-1.0, 1.0);
            let r = (right * master).clamp(-1.0, 1.0);
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());

            let start = frame_index * channels;
            write_stereo(&mut output[start..start + channels], l, r);
            if let Some(frame) = voice_bus
                .as_deref_mut()
                .and_then(|voice| voice.get_mut(start..start + channels))
            {
                let mic = (mic_sample * master).clamp(-1.0, 1.0);
                write_stereo(frame, mic, mic);
            }
        }

        self.level_l = peak_l.max(self.level_l * 0.82);
        self.level_r = peak_r.max(self.level_r * 0.82);
        self.mic_level = (self.mic_level * 0.90).min(2.0);

        self.voices.retain(|voice| !voice.finished);
        self.refresh_current();
    }

    fn snapshot(&mut self) -> AudioSnapshot {
        self.refresh_current();
        AudioSnapshot {
            output_level_l: self.level_l,
            output_level_r: self.level_r,
            mic_level: self.mic_level,
            playing: self
                .voices
                .iter()
                .filter(|voice| !voice.finished)
                .map(|voice| voice.sound_id)
                .collect(),
            current: self.current.as_ref().and_then(|current| {
                self.voices
                    .iter()
                    .find(|voice| voice.token == current.token)
                    .map(|voice| TransportSnapshot {
                        sound_id: current.sound_id,
                        position: (voice.position / voice.decoded.sample_rate as f64) as f32,
                        duration: current.duration,
                        speed: current.speed,
                    })
            }),
            paused: self.paused,
        }
    }
}

pub struct AudioEngine {
    mixer: Arc<Mutex<Mixer>>,
    output_stream: Option<Stream>,
    input_stream: Option<Stream>,
    monitor_stream: Option<Stream>,
    output_name: Option<String>,
    input_name: Option<String>,
    monitor_name: Option<String>,
    monitor_ring: Arc<Mutex<MonitorRing>>,
    monitor_active: Arc<AtomicBool>,
    /// Sounds go to the headphones and the microphone gets only the voice.
    monitor_only: Arc<AtomicBool>,
}

impl AudioEngine {
    pub fn new() -> Self {
        Self {
            mixer: Arc::new(Mutex::new(Mixer::default())),
            output_stream: None,
            input_stream: None,
            monitor_stream: None,
            output_name: None,
            input_name: None,
            monitor_name: None,
            monitor_ring: Arc::new(Mutex::new(MonitorRing::default())),
            monitor_active: Arc::new(AtomicBool::new(false)),
            monitor_only: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_output_device(&mut self, device_name: &str) -> Result<String> {
        let (device, resolved_name) = find_output_device(device_name)?;
        let supported = device
            .default_output_config()
            .with_context(|| format!("Could not read the configuration for {resolved_name}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let monitor = MonitorRoute {
            ring: self.monitor_ring.clone(),
            active: self.monitor_active.clone(),
            only: self.monitor_only.clone(),
        };

        let stream = match format {
            SampleFormat::F32 => build_output_stream::<f32>(
                &device,
                &config,
                self.mixer.clone(),
                monitor.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::I16 => build_output_stream::<i16>(
                &device,
                &config,
                self.mixer.clone(),
                monitor.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::U16 => build_output_stream::<u16>(
                &device,
                &config,
                self.mixer.clone(),
                monitor,
                channels,
                sample_rate,
            )?,
            other => bail!("Unsupported output format: {other:?}"),
        };

        {
            let mut mixer = lock_mixer(&self.mixer);
            mixer.set_output_format(sample_rate, channels);
        }
        stream
            .play()
            .with_context(|| format!("Could not start {resolved_name}"))?;

        self.output_stream = Some(stream);
        self.output_name = Some(resolved_name.clone());
        if self.monitor_name.as_deref() == Some(resolved_name.as_str()) {
            self.clear_monitor_device();
        }
        Ok(resolved_name)
    }

    pub fn set_monitor_device(&mut self, device_name: &str) -> Result<String> {
        if self.output_name.as_deref() == Some(device_name) {
            bail!("The headphone monitor must differ from the main output");
        }
        let (device, resolved_name) = find_output_device(device_name)?;
        let supported = device
            .default_output_config()
            .with_context(|| format!("Could not read the configuration for {resolved_name}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let stream = match format {
            SampleFormat::F32 => build_monitor_stream::<f32>(
                &device,
                &config,
                self.monitor_ring.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::I16 => build_monitor_stream::<i16>(
                &device,
                &config,
                self.monitor_ring.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::U16 => build_monitor_stream::<u16>(
                &device,
                &config,
                self.monitor_ring.clone(),
                channels,
                sample_rate,
            )?,
            other => bail!("Unsupported output format: {other:?}"),
        };

        {
            let mut ring = lock_monitor(&self.monitor_ring);
            ring.reset();
        }
        self.monitor_active.store(false, Ordering::Relaxed);
        self.monitor_stream = Some(stream);
        self.monitor_name = Some(resolved_name.clone());
        if let Err(error) = self
            .monitor_stream
            .as_ref()
            .expect("monitor stream just installed")
            .play()
            .with_context(|| format!("Could not start the monitor on {resolved_name}"))
        {
            self.monitor_stream = None;
            self.monitor_name = None;
            return Err(error);
        }
        self.monitor_active.store(true, Ordering::Relaxed);
        Ok(resolved_name)
    }

    pub fn clear_monitor_device(&mut self) {
        self.monitor_active.store(false, Ordering::Relaxed);
        self.monitor_stream = None;
        self.monitor_name = None;
        lock_monitor(&self.monitor_ring).reset();
    }

    pub fn set_input_device(&mut self, device_name: &str) -> Result<String> {
        let (device, resolved_name) = find_input_device(device_name)?;
        let supported = device
            .default_input_config()
            .with_context(|| format!("Could not read the configuration for {resolved_name}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let stream = match format {
            SampleFormat::F32 => build_input_stream::<f32>(
                &device,
                &config,
                self.mixer.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::I16 => build_input_stream::<i16>(
                &device,
                &config,
                self.mixer.clone(),
                channels,
                sample_rate,
            )?,
            SampleFormat::U16 => build_input_stream::<u16>(
                &device,
                &config,
                self.mixer.clone(),
                channels,
                sample_rate,
            )?,
            other => bail!("Unsupported input format: {other:?}"),
        };

        {
            let mut mixer = lock_mixer(&self.mixer);
            mixer.mic.input_rate = sample_rate;
            mixer.mic.output_rate = mixer.output_rate;
            mixer.mic.reset();
        }
        stream
            .play()
            .with_context(|| format!("Could not start {resolved_name}"))?;

        self.input_stream = Some(stream);
        self.input_name = Some(resolved_name.clone());
        Ok(resolved_name)
    }

    pub fn trigger(
        &self,
        sound_id: SoundId,
        decoded: Arc<DecodedSound>,
        gain: f32,
        looping: bool,
        start_ratio: f32,
        speed: f32,
    ) {
        let mut mixer = lock_mixer(&self.mixer);
        mixer.trigger(sound_id, decoded, gain, start_ratio, speed);
        mixer.set_current_looping(looping);
    }

    pub fn stop_all(&self) {
        lock_mixer(&self.mixer).stop_all();
    }

    pub fn stop_sound(&self, sound_id: SoundId) {
        lock_mixer(&self.mixer).stop_sound(sound_id);
    }

    pub fn set_paused(&self, paused: bool) {
        lock_mixer(&self.mixer).set_paused(paused);
    }

    pub fn set_master_volume(&self, volume: f32) {
        lock_mixer(&self.mixer).master_volume = volume.clamp(0.0, 1.0);
    }

    pub fn set_sound_volume(&self, sound_id: SoundId, volume: f32) {
        lock_mixer(&self.mixer).set_sound_volume(sound_id, volume);
    }

    pub fn set_playback_speed(&self, sound_id: SoundId, speed: f32) {
        lock_mixer(&self.mixer).set_playback_speed(sound_id, speed);
    }

    pub fn set_mic_monitor(&self, enabled: bool) {
        let mut mixer = lock_mixer(&self.mixer);
        let was_enabled = mixer.mic_monitor;
        mixer.mic_monitor = enabled;
        if enabled && !was_enabled {
            mixer.mic.reset();
        }
    }

    pub fn set_mic_gain(&self, gain: f32) {
        lock_mixer(&self.mixer).mic_gain = gain.clamp(0.0, 2.0);
    }

    /// Play the soundboard only on the headphones: the virtual microphone
    /// keeps the voice bus and never receives a sound.
    pub fn set_headphones_only(&self, enabled: bool) {
        self.monitor_only.store(enabled, Ordering::Relaxed);
    }

    /// Let pads stack instead of interrupting the previous one.
    pub fn set_polyphonic(&self, enabled: bool) {
        lock_mixer(&self.mixer).polyphonic = enabled;
    }

    pub fn set_current_looping(&self, looping: bool) {
        lock_mixer(&self.mixer).set_current_looping(looping);
    }

    pub fn seek_ratio(&self, ratio: f32) {
        lock_mixer(&self.mixer).seek_to(ratio);
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        lock_mixer(&self.mixer).snapshot()
    }
}

fn lock_mixer(mixer: &Arc<Mutex<Mixer>>) -> std::sync::MutexGuard<'_, Mixer> {
    mixer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_monitor(ring: &Arc<Mutex<MonitorRing>>) -> std::sync::MutexGuard<'_, MonitorRing> {
    ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The handles the output stream needs to drive the headphone monitor.
#[derive(Clone)]
struct MonitorRoute {
    ring: Arc<Mutex<MonitorRing>>,
    active: Arc<AtomicBool>,
    /// Sounds go to the headphones and the output carries only the voice.
    only: Arc<AtomicBool>,
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    monitor: MonitorRoute,
    channels: u16,
    sample_rate: u32,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let MonitorRoute {
        ring: monitor_ring,
        active: monitor_active,
        only: monitor_only,
    } = monitor;
    let channels = channels.max(1) as usize;
    let mut stereo = vec![0.0f32; 4096 * 2];
    let mut voice = vec![0.0f32; 4096 * 2];
    let stream = device
        .build_output_stream(
            *config,
            move |data: &mut [T], _| {
                let frames = data.len() / channels;
                if frames == 0 {
                    return;
                }
                let stereo_len = frames * 2;
                if stereo.len() < stereo_len {
                    stereo.resize(stereo_len, 0.0);
                }
                if voice.len() < stereo_len {
                    voice.resize(stereo_len, 0.0);
                }

                // Headphones-only only makes sense while the headphones are
                // actually playing; without them the program would vanish.
                let monitor_on = monitor_active.load(Ordering::Relaxed);
                let headphones_only = monitor_only.load(Ordering::Relaxed) && monitor_on;

                {
                    let mut mixer = lock_mixer(&mixer);
                    if headphones_only {
                        mixer.render_with_voice_bus(
                            &mut stereo[..stereo_len],
                            Some(&mut voice[..stereo_len]),
                            2,
                            sample_rate,
                        );
                    } else {
                        mixer.render(&mut stereo[..stereo_len], 2, sample_rate);
                    }
                }

                // The headphones hear the whole program either way.
                if monitor_on {
                    lock_monitor(&monitor_ring).push_block(&stereo[..stereo_len], sample_rate);
                }

                let program = if headphones_only {
                    &voice[..stereo_len]
                } else {
                    &stereo[..stereo_len]
                };

                for (out_frame, source) in data.chunks_mut(channels).zip(program.chunks_exact(2)) {
                    let left = source[0].clamp(-1.0, 1.0);
                    let right = source[1].clamp(-1.0, 1.0);
                    if channels == 1 {
                        out_frame[0] = T::from_sample((left + right) * 0.5);
                    } else {
                        out_frame[0] = T::from_sample(left);
                        out_frame[1] = T::from_sample(right);
                        for out in &mut out_frame[2..] {
                            *out = T::from_sample(0.0);
                        }
                    }
                }
            },
            |error| eprintln!("Output audio: {error}"),
            Some(Duration::from_millis(500)),
        )
        .context("Could not open the output audio stream")?;
    stream.play().context("Could not start the audio stream")?;
    Ok(stream)
}

fn build_monitor_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    monitor_ring: Arc<Mutex<MonitorRing>>,
    channels: u16,
    sample_rate: u32,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = channels.max(1) as usize;
    let mut stereo = vec![0.0f32; 4096 * 2];
    let stream = device
        .build_output_stream(
            *config,
            move |data: &mut [T], _| {
                let frames = data.len() / channels;
                if frames == 0 {
                    return;
                }
                let stereo_len = frames * 2;
                if stereo.len() < stereo_len {
                    stereo.resize(stereo_len, 0.0);
                }
                {
                    let mut ring = lock_monitor(&monitor_ring);
                    ring.fill_stereo(&mut stereo[..stereo_len], sample_rate);
                }

                for (out_frame, source) in data
                    .chunks_mut(channels)
                    .zip(stereo[..stereo_len].chunks_exact(2))
                {
                    let left = source[0].clamp(-1.0, 1.0);
                    let right = source[1].clamp(-1.0, 1.0);
                    if channels == 1 {
                        out_frame[0] = T::from_sample((left + right) * 0.5);
                    } else {
                        out_frame[0] = T::from_sample(left);
                        out_frame[1] = T::from_sample(right);
                        for out in &mut out_frame[2..] {
                            *out = T::from_sample(0.0);
                        }
                    }
                }
            },
            |error| eprintln!("Headphone monitor: {error}"),
            Some(Duration::from_millis(500)),
        )
        .context("Could not open the headphone monitor")?;
    Ok(stream)
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    channels: u16,
    _sample_rate: u32,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut scratch = vec![0.0f32; 4096];
    let channels = channels.max(1) as usize;
    let stream = device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                if scratch.len() < data.len() {
                    scratch.resize(data.len(), 0.0);
                }
                for (dst, source) in scratch.iter_mut().zip(data.iter()) {
                    *dst = f32::from_sample(*source);
                }
                let mut mixer = lock_mixer(&mixer);
                mixer.push_mic(&scratch[..data.len()], channels);
            },
            |error| eprintln!("Microphone: {error}"),
            Some(Duration::from_millis(500)),
        )
        .context("Could not open the microphone stream")?;
    stream.play().context("Could not start the microphone")?;
    Ok(stream)
}

fn find_output_device(name: &str) -> Result<(cpal::Device, String)> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .context("Could not enumerate outputs")?;
    for device in devices {
        let device_name = device.to_string();
        if device_name == name {
            return Ok((device, device_name));
        }
    }
    bail!("Audio output {name} is no longer available")
}

fn find_input_device(name: &str) -> Result<(cpal::Device, String)> {
    let host = cpal::default_host();
    let devices = host.input_devices().context("Could not enumerate inputs")?;
    for device in devices {
        let device_name = device.to_string();
        if device_name == name {
            return Ok((device, device_name));
        }
    }
    bail!("Microphone {name} is no longer available")
}

pub fn list_output_devices() -> Vec<DeviceChoice> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .map(|device| device.to_string());
    let mut choices = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            let name = device.to_string();
            let is_default = default_name.as_deref() == Some(name.as_str());
            choices.push(DeviceChoice {
                is_virtual: looks_virtual(&name),
                is_default,
                name,
            });
        }
    }
    choices.sort_by(|a, b| {
        (!a.is_virtual, !a.is_default, &a.name).cmp(&(!b.is_virtual, !b.is_default, &b.name))
    });
    choices
}

pub fn list_input_devices() -> Vec<DeviceChoice> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().map(|device| device.to_string());
    let mut choices = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            let name = device.to_string();
            let is_default = default_name.as_deref() == Some(name.as_str());
            choices.push(DeviceChoice {
                is_virtual: looks_virtual(&name),
                is_default,
                name,
            });
        }
    }
    choices.sort_by(|a, b| {
        (a.is_virtual, !a.is_default, &a.name).cmp(&(b.is_virtual, !b.is_default, &b.name))
    });
    choices
}

pub fn looks_virtual(name: &str) -> bool {
    let normalized = name.to_lowercase();
    [
        "cable input",
        "cable output",
        "vb-audio",
        "voicemeeter",
        "virtual cable",
        "virtual audio",
        "virtual mixer",
        "sonar",
        "blackhole",
        "soundflower",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// Decode an audio file to stereo f32, plus a compact peak waveform for the UI.
///
/// This is intentionally independent from GPUI so it can run on the background
/// executor without holding up a frame.
pub fn decode_audio(path: &Path) -> Result<DecodedSound> {
    let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let media = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(&extension.to_ascii_lowercase());
    }

    let probed = symphonia::default::get_probe()
        .probe(
            &hint,
            media,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("Unrecognized audio format: {}", path.display()))?;
    let mut format = probed;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("No audio track in {}", path.display()))?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| anyhow!("Missing audio parameters in {}", path.display()))?;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
        .context("Unsupported audio codec")?;

    let mut stereo = Vec::<f32>::new();
    let mut sample_rate = 0u32;
    let mut packet_samples = Vec::<f32>::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error).context("Failed to read the audio file"),
        };

        if packet.track_id != track_id {
            continue;
        }

        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error).context("Audio decoding failed"),
        };

        let spec = audio.spec();
        sample_rate = spec.rate();
        let channels = spec.channels().count().max(1);
        packet_samples.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut packet_samples);

        if channels == 1 {
            for sample in &packet_samples {
                stereo.push(*sample);
                stereo.push(*sample);
            }
        } else {
            for frame in packet_samples.chunks(channels) {
                stereo.push(frame[0]);
                stereo.push(if channels > 1 { frame[1] } else { frame[0] });
            }
        }
    }

    if stereo.is_empty() || sample_rate == 0 {
        bail!("The file contains no decodable audio");
    }

    let frames = stereo.len() / 2;
    let duration = frames as f32 / sample_rate as f32;
    let waveform = build_waveform(&stereo);

    Ok(DecodedSound {
        samples: Arc::from(stereo.into_boxed_slice()),
        sample_rate,
        duration,
        waveform: Arc::from(waveform.into_boxed_slice()),
    })
}

fn build_waveform(stereo: &[f32]) -> Vec<f32> {
    let frames = stereo.len() / 2;
    if frames == 0 {
        return Vec::new();
    }
    let mut peaks = vec![0.0f32; WAVEFORM_BUCKETS];
    for frame in 0..frames {
        let bucket = frame * WAVEFORM_BUCKETS / frames;
        let left = stereo[frame * 2].abs();
        let right = stereo[frame * 2 + 1].abs();
        peaks[bucket] = peaks[bucket].max(left.max(right));
    }
    let loudest = peaks.iter().fold(0.0f32, |max, peak| max.max(*peak));
    if loudest > 0.0001 {
        for peak in &mut peaks {
            *peak = (*peak / loudest).sqrt().clamp(0.0, 1.0);
        }
    }
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_devices_are_recognized_by_name() {
        assert!(looks_virtual("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(looks_virtual("VoiceMeeter Input"));
        assert!(!looks_virtual("Speakers (Realtek(R) Audio)"));
    }

    #[test]
    fn waveform_has_expected_shape() {
        let mut stereo = Vec::new();
        for index in 0..4800 {
            let value = (index as f32 / 4800.0) * 0.5;
            stereo.push(value);
            stereo.push(value);
        }
        let waveform = build_waveform(&stereo);
        assert_eq!(waveform.len(), WAVEFORM_BUCKETS);
        assert!(waveform.iter().all(|peak| (0.0..=1.0).contains(peak)));
        assert!(waveform.last().unwrap() > waveform.first().unwrap());
    }

    fn test_tone(frames: usize) -> DecodedSound {
        let samples = vec![0.25f32; frames * 2];
        DecodedSound {
            samples: Arc::from(samples.into_boxed_slice()),
            sample_rate: 48_000,
            duration: frames as f32 / 48_000.0,
            waveform: Arc::from(vec![1.0f32; WAVEFORM_BUCKETS].into_boxed_slice()),
        }
    }

    #[test]
    fn the_mixer_renders_and_clears_finished_voices() {
        let mut mixer = Mixer::default();
        mixer.trigger(42, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        assert!(mixer.snapshot().current.is_some());

        let mut first = vec![0.0f32; 512 * 2];
        mixer.render(&mut first, 2, 48_000);
        assert!(first.iter().any(|sample| sample.abs() > 0.05));
        assert!(mixer.level_l > 0.0);

        let mut rest = vec![0.0f32; 4800 * 2];
        mixer.render(&mut rest, 2, 48_000);
        assert!(mixer.snapshot().current.is_none());
    }

    #[test]
    fn headphones_only_keeps_sounds_out_of_the_microphone_bus() {
        let mut mixer = Mixer::default();
        mixer.set_output_format(48_000, 2);
        mixer.mic_monitor = true;
        mixer.mic_gain = 0.5;
        mixer.mic.input_rate = 48_000;
        // Prebuffer a constant voice so the voice bus has something to send.
        mixer.push_mic(&vec![0.8f32; 4800], 1);

        // A clip alternating every frame shows what leaked into which bus.
        let alternating = DecodedSound {
            samples: Arc::from(
                (0..4800)
                    .flat_map(|index| {
                        let value = if index % 2 == 0 { 0.4f32 } else { -0.4f32 };
                        [value, value]
                    })
                    .collect::<Vec<f32>>()
                    .into_boxed_slice(),
            ),
            sample_rate: 48_000,
            duration: 0.1,
            waveform: Arc::from(vec![1.0f32; WAVEFORM_BUCKETS].into_boxed_slice()),
        };
        mixer.trigger(3, Arc::new(alternating), 1.0, 0.0, 1.0);

        let mut program = vec![0.0f32; 512 * 2];
        let mut voice = vec![0.0f32; 512 * 2];
        mixer.render_with_voice_bus(&mut program, Some(&mut voice), 2, 48_000);

        // The headphones get the clip: the program alternates loudly.
        let loudest = program.iter().copied().fold(f32::MIN, f32::max);
        let quietest = program.iter().copied().fold(f32::MAX, f32::min);
        assert!(loudest - quietest > 0.3);

        // The microphone gets the voice only: constant, not a trace of the clip.
        let voice_level = voice[0];
        assert!(voice_level > 0.1);
        assert!(
            voice
                .iter()
                .all(|sample| (sample - voice_level).abs() < 1e-6)
        );

        // Without the voice mix the microphone falls silent while the
        // headphones keep playing.
        mixer.mic_monitor = false;
        program.fill(0.0);
        voice.fill(0.0);
        mixer.render_with_voice_bus(&mut program, Some(&mut voice), 2, 48_000);
        assert!(voice.iter().all(|sample| *sample == 0.0));
        assert!(program.iter().any(|sample| sample.abs() > 0.1));
    }

    #[test]
    fn polyphonic_mixers_keep_the_previous_voice() {
        let mut mixer = Mixer::default();
        mixer.trigger(1, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        mixer.trigger(2, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        assert_eq!(mixer.voices.len(), 1, "exclusive by default");

        mixer.polyphonic = true;
        mixer.trigger(1, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        mixer.trigger(2, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        assert_eq!(mixer.voices.len(), 2, "pads stack");
        assert_eq!(mixer.snapshot().playing.len(), 2);

        // Triggering the same pad again restarts its voice instead of doubling it.
        mixer.trigger(2, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        assert_eq!(mixer.voices.len(), 2);
        assert_eq!(mixer.snapshot().current.unwrap().sound_id, 2);
    }

    #[test]
    fn the_transport_promotes_the_next_voice_when_one_ends() {
        let mut mixer = Mixer {
            polyphonic: true,
            ..Mixer::default()
        };
        mixer.trigger(1, Arc::new(test_tone(48_000)), 1.0, 0.0, 1.0);
        mixer.trigger(2, Arc::new(test_tone(2_400)), 1.0, 0.0, 1.0);
        assert_eq!(mixer.snapshot().current.unwrap().sound_id, 2);

        let mut buffer = vec![0.0f32; 4800 * 2];
        mixer.render(&mut buffer, 2, 48_000);
        let snapshot = mixer.snapshot();
        assert_eq!(
            snapshot
                .current
                .expect("the long clip is still on air")
                .sound_id,
            1
        );
        assert_eq!(snapshot.playing, vec![1]);
    }

    #[test]
    fn each_pad_has_its_own_playback_speed() {
        for (speed, expected) in [(0.25, 0.025), (1.0, 0.1), (2.0, 0.2)] {
            let mut mixer = Mixer::default();
            mixer.trigger(5, Arc::new(test_tone(48_000)), 1.0, 0.0, speed);

            let mut buffer = vec![0.0f32; 4800 * 2];
            mixer.render(&mut buffer, 2, 48_000);

            let position = mixer
                .snapshot()
                .current
                .expect("the clip is still on air")
                .position;
            assert!(
                (position - expected).abs() < 0.001,
                "{speed}× advanced to {position}, expected {expected}"
            );
        }

        // Overlapping voices keep independent rates.
        let mut mixer = Mixer {
            polyphonic: true,
            ..Mixer::default()
        };
        mixer.trigger(1, Arc::new(test_tone(48_000)), 1.0, 0.0, 2.0);
        mixer.trigger(2, Arc::new(test_tone(48_000)), 1.0, 0.0, 0.5);

        let mut buffer = vec![0.0f32; 4800 * 2];
        mixer.render(&mut buffer, 2, 48_000);
        let current = mixer.snapshot().current.expect("both voices are on air");
        assert_eq!(current.sound_id, 2);
        assert!((current.position - 0.05).abs() < 0.001);
        assert_eq!(current.speed, 0.5);

        // Dropping the newest voice promotes the slower one with its own rate.
        mixer.stop_sound(2);
        let current = mixer.snapshot().current.expect("the first voice survives");
        assert_eq!(current.sound_id, 1);
        assert!((current.position - 0.2).abs() < 0.001);
        assert_eq!(current.speed, 2.0);

        // A live change touches that pad only and snaps to the offered grid.
        mixer.set_playback_speed(1, 1.1);
        let current = mixer.snapshot().current.unwrap();
        assert_eq!(current.speed, 1.0);
        let before = current.position;
        mixer.render(&mut buffer, 2, 48_000);
        let after = mixer.snapshot().current.unwrap().position;
        assert!((after - before - 0.1).abs() < 0.001);
    }

    #[test]
    fn pause_holds_the_playhead_and_writes_silence() {
        let mut mixer = Mixer::default();
        mixer.trigger(7, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        mixer.set_paused(true);

        let mut buffer = vec![0.0f32; 512 * 2];
        mixer.render(&mut buffer, 2, 48_000);
        assert!(buffer.iter().all(|sample| *sample == 0.0));
        assert_eq!(mixer.snapshot().current.unwrap().position, 0.0);

        mixer.set_paused(false);
        mixer.render(&mut buffer, 2, 48_000);
        assert!(buffer.iter().any(|sample| sample.abs() > 0.05));
    }

    #[test]
    fn a_trigger_can_open_at_an_offset() {
        let mut mixer = Mixer::default();
        // A 0.1s tone opened at the halfway point starts at 0.05s.
        mixer.trigger(11, Arc::new(test_tone(4800)), 1.0, 0.5, 1.0);
        let current = mixer.snapshot().current.unwrap();
        assert!((current.position - 0.05).abs() < 0.001);

        // The offset is clamped so the voice always has a frame to play.
        mixer.trigger(12, Arc::new(test_tone(4800)), 1.0, 1.0, 1.0);
        let current = mixer.snapshot().current.unwrap();
        assert!(current.position < current.duration);
    }

    #[test]
    fn looping_keeps_the_transport_alive_past_the_end() {
        let mut mixer = Mixer::default();
        mixer.trigger(9, Arc::new(test_tone(480)), 1.0, 0.0, 1.0);
        mixer.set_current_looping(true);
        let mut buffer = vec![0.0f32; 4800 * 2];
        mixer.render(&mut buffer, 2, 48_000);
        let current = mixer.snapshot().current;
        assert!(current.is_some());
        assert!(current.unwrap().position < 0.1);
    }

    fn write_test_wav(path: &Path) {
        let sample_rate = 8_000u32;
        let samples: Vec<i16> = (0..800)
            .map(|index| {
                let phase = index as f32 / sample_rate as f32 * 440.0 * std::f32::consts::TAU;
                (phase.sin() * 8_000.0) as i16
            })
            .collect();
        let data_len = (samples.len() * 2) as u32;
        let mut data = Vec::with_capacity(44 + data_len as usize);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&sample_rate.to_le_bytes());
        data.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn wav_decoding_produces_stereo_pcm() {
        let path = std::env::temp_dir().join("simple-soundboard-audio-test.wav");
        write_test_wav(&path);
        let decoded = decode_audio(&path).unwrap();
        assert_eq!(decoded.sample_rate, 8_000);
        assert!((decoded.duration - 0.1).abs() < 0.01);
        assert_eq!(decoded.samples.len(), 1600);
        assert!(decoded.waveform.iter().any(|peak| *peak > 0.0));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mp3_decoding_works_with_the_mp3_feature() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine.mp3");
        let decoded = decode_audio(&path).unwrap();
        assert!(decoded.sample_rate > 0);
        assert!(decoded.duration > 0.2);
        assert!(!decoded.samples.is_empty());
        assert!(!decoded.waveform.is_empty());
    }

    #[test]
    fn per_sound_gain_updates_a_live_voice() {
        let mut mixer = Mixer::default();
        mixer.trigger(31, Arc::new(test_tone(480)), 1.0, 0.0, 1.0);
        mixer.set_sound_volume(31, 0.25);
        assert!((mixer.voices[0].gain - 0.25).abs() < 0.0001);
        mixer.set_sound_volume(31, 0.75);
        assert!((mixer.voices[0].gain - 0.75).abs() < 0.0001);
    }

    #[test]
    fn a_new_cue_silences_the_previous_one() {
        let mut mixer = Mixer::default();
        mixer.trigger(1, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        mixer.trigger(2, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        assert_eq!(mixer.voices.len(), 1);
        assert_eq!(mixer.snapshot().current.unwrap().sound_id, 2);
    }

    #[test]
    fn trim_cuts_the_decoded_audio_and_waveform() {
        let sound = test_tone(4800);
        let trimmed = sound.trimmed(0.02, 0.08);
        assert!((trimmed.duration - 0.06).abs() < 0.002);
        assert_eq!(trimmed.samples.len(), (0.06 * 48_000.0 * 2.0) as usize);
        assert_eq!(trimmed.waveform.len(), WAVEFORM_BUCKETS);
    }

    #[test]
    fn monitor_ring_delivers_synced_stereo_after_prebuffering() {
        let mut ring = MonitorRing::default();
        let mut source = Vec::with_capacity(4800 * 2);
        for _ in 0..4800 {
            source.push(0.5);
            source.push(-0.5);
        }
        ring.push_block(&source, 48_000);
        let mut out = vec![0.0f32; 512 * 2];
        ring.fill_stereo(&mut out, 48_000);
        assert!(out.iter().any(|sample| *sample != 0.0));
        assert!((out[0] - 0.5).abs() < 0.001);
        assert!((out[1] + 0.5).abs() < 0.001);
    }

    #[test]
    fn a_finished_voice_cannot_keep_the_transport_alive() {
        let mut mixer = Mixer::default();
        mixer.trigger(77, Arc::new(test_tone(4800)), 1.0, 0.0, 1.0);
        mixer.voices[0].finished = true;
        assert!(mixer.snapshot().current.is_none());
    }

    #[test]
    fn a_two_second_track_finishes_even_at_a_different_sample_rate() {
        let frames = 88_200usize;
        let sound = DecodedSound {
            samples: Arc::from(vec![0.1f32; frames * 2].into_boxed_slice()),
            sample_rate: 44_100,
            duration: 2.0,
            waveform: Arc::from(vec![0.1f32; WAVEFORM_BUCKETS].into_boxed_slice()),
        };
        let mut mixer = Mixer::default();
        mixer.trigger(78, Arc::new(sound), 1.0, 0.0, 1.0);
        let mut buffer = vec![0.0f32; 48_000 * 2 * 3];
        mixer.render(&mut buffer, 2, 48_000);
        assert!(mixer.snapshot().current.is_none());
    }
}
