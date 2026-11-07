use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui_kit::{Keystroke, SharedString};
use serde::{Deserialize, Serialize};

use crate::audio::DecodedSound;

pub type SoundId = u64;

/// Loudest a pad volume may go. Values above 100% are a boost for quiet
/// clips; the mixer still clamps the final sample to the output range.
pub const MAX_SOUND_VOLUME: f32 = 2.0;

/// Playback rates offered by each pad, slowest first.
pub const PLAYBACK_SPEEDS: [f32; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// Slowest and fastest rate a pad can play at.
pub const MIN_PLAYBACK_SPEED: f32 = PLAYBACK_SPEEDS[0];
pub const MAX_PLAYBACK_SPEED: f32 = PLAYBACK_SPEEDS[PLAYBACK_SPEEDS.len() - 1];

/// Snap a rate to the closest entry in [`PLAYBACK_SPEEDS`].
///
/// Saved JSON is user-editable and the mixer must never see an arbitrary
/// rate: finite values snap to the nearest offered speed, while broken values
/// fall back to normal speed.
pub fn normalize_playback_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return 1.0;
    }
    PLAYBACK_SPEEDS
        .into_iter()
        .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
        .unwrap_or(1.0)
}

/// Format a playback rate without trailing zeros: `0.25×`, `1×`, `1.5×`.
pub fn format_playback_speed(speed: f32) -> String {
    let speed = normalize_playback_speed(speed);
    let hundredths = (speed * 100.0).round() as u32;
    if hundredths.is_multiple_of(100) {
        format!("{speed:.0}×")
    } else if hundredths.is_multiple_of(50) {
        format!("{speed:.1}×")
    } else {
        format!("{speed:.2}×")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyShortcut {
    #[serde(default)]
    pub control: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    pub key: String,
}

impl KeyShortcut {
    pub fn from_keystroke(keystroke: &Keystroke) -> Option<Self> {
        let key = keystroke.key.trim().to_ascii_lowercase();
        if key.is_empty() || is_modifier_key(&key) || keystroke.modifiers.platform {
            return None;
        }

        let shortcut = Self {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            key,
        };
        let is_function_key = shortcut.key.len() > 1
            && shortcut.key.starts_with('f')
            && shortcut.key[1..]
                .chars()
                .all(|character| character.is_ascii_digit());
        if (!shortcut.control && !shortcut.alt && !is_function_key) || shortcut.is_reserved() {
            return None;
        }

        Some(shortcut)
    }

    pub fn label(&self) -> String {
        let mut label = String::new();
        if self.control {
            label.push_str("Ctrl+");
        }
        if self.alt {
            label.push_str("Alt+");
        }
        if self.shift {
            label.push_str("Shift+");
        }
        label.push_str(&display_key(&self.key));
        label
    }

    fn is_reserved(&self) -> bool {
        let key = self.key.as_str();
        (self.control
            && !self.alt
            && !self.shift
            && matches!(key, "p" | "o" | "l" | "f" | "escape"))
            || (self.alt && key == "f4")
            || (self.control && self.alt && key == "delete")
    }
}

pub(crate) fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "control"
            | "ctrl"
            | "alt"
            | "shift"
            | "platform"
            | "super"
            | "win"
            | "cmd"
            | "fn"
            | "function"
    )
}

fn display_key(key: &str) -> String {
    match key {
        "space" => "Space".into(),
        "tab" => "Tab".into(),
        "enter" => "Enter".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Delete".into(),
        "insert" => "Insert".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "escape" => "Esc".into(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut characters = key.chars();
            match characters.next() {
                Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
                None => key.to_string(),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum SoundStatus {
    Loading,
    Ready,
    Failed(SharedString),
}

#[derive(Clone, Debug)]
pub struct Sound {
    pub id: SoundId,
    pub path: PathBuf,
    pub title: SharedString,
    pub volume: f32,
    /// Playback rate for this pad only, snapped to the offered speeds.
    pub playback_speed: f32,
    pub status: SoundStatus,
    /// The complete decoded file. Retained so the clip can be trimmed more
    /// than once without decoding the file again.
    pub source: Option<Arc<DecodedSound>>,
    /// The active playback result after trimming.
    pub decoded: Option<Arc<DecodedSound>>,
    /// Trim boundaries in seconds on the original file.
    pub trim_start: f32,
    pub trim_end: f32,
    pub shortcut: Option<KeyShortcut>,
}

impl Sound {
    pub fn new(path: PathBuf, volume: f32) -> Self {
        let id = sound_id(&path);
        let title = title_from_path(&path);
        Self {
            id,
            path,
            title,
            volume: volume.clamp(0.0, MAX_SOUND_VOLUME),
            playback_speed: 1.0,
            status: SoundStatus::Loading,
            source: None,
            decoded: None,
            trim_start: 0.0,
            trim_end: 0.0,
            shortcut: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, SoundStatus::Ready)
    }

    pub fn duration(&self) -> f32 {
        self.decoded.as_ref().map(|s| s.duration).unwrap_or(0.0)
    }

    pub fn waveform(&self) -> &[f32] {
        self.decoded
            .as_ref()
            .map(|s| s.waveform.as_ref())
            .unwrap_or(&[])
    }

    pub fn full_duration(&self) -> f32 {
        self.source.as_ref().map(|s| s.duration).unwrap_or(0.0)
    }

    /// Cut a region out of the original decoded audio and make it the
    /// playable version of this sound.
    pub fn apply_trim(&mut self, start_seconds: f32, end_seconds: f32) -> bool {
        let Some(source) = self.source.as_ref() else {
            return false;
        };
        let duration = source.duration;
        let start = start_seconds.clamp(0.0, duration);
        let end = end_seconds.clamp(0.0, duration);
        if end - start < 0.05 {
            return false;
        }
        self.trim_start = start;
        self.trim_end = end;
        self.decoded = Some(Arc::new(source.trimmed(start, end)));
        true
    }

    pub fn format_label(&self) -> SharedString {
        self.path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_uppercase())
            .unwrap_or_else(|| "AUDIO".to_string())
            .into()
    }

    pub fn location_label(&self) -> SharedString {
        self.path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("Local folder")
            .into()
    }

    pub fn set_source(&mut self, source: Arc<DecodedSound>) {
        self.source = Some(source.clone());
        self.status = SoundStatus::Ready;
        let duration = source.duration;
        let start = self.trim_start.clamp(0.0, duration);
        let mut end = self.trim_end.clamp(0.0, duration);
        if end - start < 0.05 {
            end = duration;
        }
        self.trim_start = start.min(duration);
        self.trim_end = end;
        self.decoded = Some(Arc::new(source.trimmed(self.trim_start, self.trim_end)));
    }

    pub fn set_failed(&mut self, message: impl Into<SharedString>) {
        self.status = SoundStatus::Failed(message.into());
        self.source = None;
        self.decoded = None;
    }
}

pub fn sound_id(path: &Path) -> SoundId {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn title_from_path(path: &Path) -> SharedString {
    let raw = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled sound");
    let title = raw
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "Untitled sound".into()
    } else {
        title.into()
    }
}

pub fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp3" | "wav" | "ogg" | "flac"
            )
        })
        .unwrap_or(false)
}

pub fn format_duration(seconds: f32) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "--:--".into();
    }
    let total = seconds.round().max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

pub fn format_elapsed(seconds: f32) -> String {
    let total = seconds.max(0.0) as u64;
    let minutes = total / 60;
    let secs = total % 60;
    format!("{minutes:02}:{secs:02}")
}

pub fn format_time_precise(seconds: f32) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00.0".into();
    }
    let minutes = (seconds / 60.0).floor() as u64;
    let secs = seconds - minutes as f32 * 60.0;
    format!("{minutes}:{secs:04.1}")
}

pub fn format_percent(value: f32) -> String {
    format!(
        "{:>3.0}%",
        (value.clamp(0.0, MAX_SOUND_VOLUME) * 100.0).round()
    )
}

pub fn normalize_peak(value: f32) -> f32 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * value.log10();
    ((db + 48.0) / 48.0).clamp(0.0, 1.0)
}

pub fn peak_to_db(value: f32) -> Option<f32> {
    if !value.is_finite() || value <= 0.0001 {
        None
    } else {
        Some(20.0 * value.log10())
    }
}
#[cfg(test)]
mod tests {
    use super::{
        KeyShortcut, PLAYBACK_SPEEDS, Sound, format_percent, format_playback_speed,
        normalize_playback_speed,
    };
    use gpui_kit::{Keystroke, Modifiers};
    use std::path::PathBuf;

    fn keystroke(key: &str, control: bool, alt: bool, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control,
                alt,
                shift,
                platform: false,
                function: false,
            },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn shortcuts_require_a_modifier_or_a_function_key() {
        assert!(KeyShortcut::from_keystroke(&keystroke("1", true, true, false)).is_some());
        assert!(KeyShortcut::from_keystroke(&keystroke("f5", false, false, false)).is_some());
        assert!(KeyShortcut::from_keystroke(&keystroke("a", false, false, false)).is_none());
        assert!(KeyShortcut::from_keystroke(&keystroke("control", true, false, false)).is_none());
    }

    #[test]
    fn playback_speeds_snap_to_the_offered_rates() {
        for (speed, expected) in [
            (0.1, 0.25),
            (0.25, 0.25),
            (0.37, 0.25),
            (0.4, 0.5),
            (0.6, 0.5),
            (0.65, 0.75),
            (0.9, 1.0),
            (1.12, 1.0),
            (1.13, 1.25),
            (1.49, 1.5),
            (1.9, 2.0),
            (3.0, 2.0),
            (f32::NAN, 1.0),
            (f32::INFINITY, 1.0),
        ] {
            assert_eq!(normalize_playback_speed(speed), expected, "{speed}");
        }
    }

    #[test]
    fn playback_speed_labels_skip_trailing_zeros() {
        assert_eq!(format_playback_speed(0.25), "0.25×");
        assert_eq!(format_playback_speed(0.5), "0.5×");
        assert_eq!(format_playback_speed(0.75), "0.75×");
        assert_eq!(format_playback_speed(1.0), "1×");
        assert_eq!(format_playback_speed(1.25), "1.25×");
        assert_eq!(format_playback_speed(1.5), "1.5×");
        assert_eq!(format_playback_speed(2.0), "2×");
        assert_eq!(PLAYBACK_SPEEDS.len(), 8);
    }

    #[test]
    fn pad_volume_accepts_a_boost_up_to_two_hundred_percent() {
        let sound = Sound::new(PathBuf::from("clip.mp3"), 3.0);
        assert_eq!(sound.volume, 2.0);
        assert_eq!(sound.playback_speed, 1.0);
        assert_eq!(format_percent(2.0), "200%");
        assert_eq!(format_percent(1.0), "100%");
    }

    #[test]
    fn reserved_shortcuts_are_rejected_and_labels_are_readable() {
        assert!(KeyShortcut::from_keystroke(&keystroke("p", true, false, false)).is_none());
        assert!(KeyShortcut::from_keystroke(&keystroke("f4", false, true, false)).is_none());
        let shortcut =
            KeyShortcut::from_keystroke(&keystroke("1", true, true, false)).expect("shortcut");
        assert_eq!(shortcut.label(), "Ctrl+Alt+1");
    }
}
