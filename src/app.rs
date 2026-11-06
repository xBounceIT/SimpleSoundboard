mod view;

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    IndexPath, WindowExt as _,
    input::{InputEvent, InputState},
    select::{SelectEvent, SelectState},
    slider::{SliderEvent, SliderState},
};
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use smol::Timer;

use crate::{
    audio::{self, AudioEngine, DeviceChoice, TransportSnapshot},
    model::{
        KeyShortcut, Sound, SoundId, SoundStatus, format_percent, is_supported_audio, sound_id,
    },
    update::{self, AvailableUpdate},
};

gpui_kit::actions!(
    soundboard,
    [
        PlayPause,
        StopPlayback,
        NextSound,
        PreviousSound,
        AddSounds,
        ToggleLoop,
        FocusSearch
    ]
);

const STATE_VERSION: u32 = 2;

/// Page-size choices offered by the pagination footer, in sounds per page.
pub(crate) const PAGE_SIZE_OPTIONS: [usize; 7] = [3, 6, 9, 12, 18, 24, 48];

/// Ceiling for [`PageSize::Auto`] on very tall windows.
const MAX_AUTO_PAGE_SIZE: usize = 60;

/// Narrowest a pad may get, in rem. Wider windows add columns instead of
/// stretching the same three pads across the whole library.
const MIN_TILE_WIDTH_REM: f32 = 16.0;

/// Ceiling on the Auto column count, so 4K windows still get readable pads.
const MAX_AUTO_COLUMNS: usize = 10;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sounds: Vec<PersistedSound>,
    #[serde(default)]
    selected_path: Option<String>,
    #[serde(default)]
    master_volume: Option<f32>,
    #[serde(default)]
    mic_monitor: Option<bool>,
    #[serde(default)]
    mic_gain: Option<f32>,
    #[serde(default)]
    looping: Option<bool>,
    #[serde(default)]
    output_device: Option<String>,
    #[serde(default)]
    input_device: Option<String>,
    #[serde(default)]
    monitor_device: Option<String>,
    #[serde(default)]
    monitor_enabled: Option<bool>,
    /// Sounds per page; `None` is the Auto behaviour.
    #[serde(default)]
    per_page: Option<usize>,
    /// Sounds play on the headphones only and never reach the microphone.
    #[serde(default)]
    headphones_only: Option<bool>,
    /// Let pads play on top of each other instead of interrupting.
    #[serde(default)]
    polyphonic: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSound {
    path: String,
    #[serde(default = "default_volume")]
    volume: f32,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    trim_start: Option<f32>,
    #[serde(default)]
    trim_end: Option<f32>,
    #[serde(default)]
    shortcut: Option<KeyShortcut>,
}

fn default_volume() -> f32 {
    1.0
}

#[derive(Clone, Copy, Debug)]
enum TrimDrag {
    Start,
    End,
    Window { grab: f32 },
}

/// The point of a pad waveform under the pointer, previewed before a click
/// commits it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct WaveformHover {
    sound_id: SoundId,
    /// Position inside the clip, from 0 to 1.
    ratio: f32,
}

/// Categories of the settings page, shown one per row on the left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    #[default]
    Preferences,
    /// Version, updates, release notes and where the bank is saved.
    About,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 2] = [Self::Preferences, Self::About];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Preferences => "PREFERENCES",
            Self::About => "ABOUT",
        }
    }
}

/// How many sounds the library shows on one page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PageSize {
    /// Fit as many pads as the window has room for.
    #[default]
    Auto,
    /// A size picked from [`PAGE_SIZE_OPTIONS`].
    Fixed(usize),
}

impl PageSize {
    fn from_saved(value: Option<usize>) -> Self {
        match value {
            Some(size) if PAGE_SIZE_OPTIONS.contains(&size) => Self::Fixed(size),
            _ => Self::Auto,
        }
    }

    fn saved(self) -> Option<usize> {
        match self {
            Self::Auto => None,
            Self::Fixed(size) => Some(size),
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Auto => SharedString::from("Auto"),
            Self::Fixed(size) => SharedString::from(size.to_string()),
        }
    }

    /// Position of this choice in the select, where 0 is Auto.
    fn index(self) -> usize {
        match self {
            Self::Auto => 0,
            Self::Fixed(size) => PAGE_SIZE_OPTIONS
                .iter()
                .position(|option| *option == size)
                .map(|index| index + 1)
                .unwrap_or(0),
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        if label.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        let size = label.parse::<usize>().ok()?;
        PAGE_SIZE_OPTIONS
            .contains(&size)
            .then_some(Self::Fixed(size))
    }
}

fn page_size_labels() -> Vec<SharedString> {
    std::iter::once(PageSize::Auto.label())
        .chain(
            PAGE_SIZE_OPTIONS
                .iter()
                .map(|size| SharedString::from(size.to_string())),
        )
        .collect()
}

/// Pad geometry for the current window, shared by the page-size calculation
/// and by the grid that renders the pads, so the two can never disagree.
pub(crate) struct LibraryMetrics {
    /// Pads per row. The grid only needs [`tile_width`](Self::tile_width), so
    /// this is read by the layout tests.
    #[cfg_attr(not(test), allow(dead_code))]
    columns: usize,
    /// Width of a single pad, in pixels.
    pub(crate) tile_width: f32,
    /// Sounds that fill the window with whole rows.
    per_page: usize,
}

impl LibraryMetrics {
    /// Measure the library of an actual window.
    pub(crate) fn for_window(window: &Window) -> Self {
        Self::for_viewport(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
            f32::from(window.rem_size()),
        )
    }

    /// The geometry that fills the library with whole rows of pads.
    ///
    /// The measurements mirror the rem-based layout in `view.rs`:
    ///
    /// * a 4rem title bar (`h_16`) and a 6rem transport bar (`h_24`) sit above
    ///   and below the library;
    /// * the library toolbar adds 3.75rem (`pt_5` + the 2rem search field +
    ///   `pb_2`) and the pagination footer 3rem (`py_3` around small controls);
    /// * pads are 12rem tall (`h_48`), at least [`MIN_TILE_WIDTH_REM`] wide,
    ///   separated by a 0.75rem gap (`gap_3`), inside 2rem of vertical padding
    ///   (`pt_3` + `pb_5`) and 2.5rem of horizontal padding (`px_5`).
    ///
    /// Columns are derived from a *minimum* pad width, so a wider window gains
    /// columns (and pads) instead of just widening the ones it already shows.
    ///
    /// The trim editor temporarily shrinks the listing, but Auto deliberately
    /// follows only the window so the page size stays put while trimming.
    ///
    /// Keep those values in sync if the layout around the library changes.
    fn for_viewport(width: f32, height: f32, rem: f32) -> Self {
        let rem = rem.max(1.0);
        let library_width = (width - 20.0 * rem).max(1.0); // routing panel `w_80`
        let content_width = (library_width - 2.5 * rem).max(1.0); // grid `px_5`
        let gap = 0.75 * rem;
        let min_tile_width = MIN_TILE_WIDTH_REM * rem;
        let columns = (((content_width + gap) / (min_tile_width + gap)).floor() as usize)
            .clamp(1, MAX_AUTO_COLUMNS);
        // Leave a little slack on the right of every row so fractional pixel
        // widths can never overflow and wrap a pad onto the next row.
        let usable_width = content_width * 0.99;
        let tile_width = ((usable_width - gap * (columns - 1) as f32) / columns as f32).max(1.0);

        let grid_height = (height - 16.75 * rem).max(0.0);
        let content_height = (grid_height - 2.0 * rem).max(0.0);
        let rows = (((content_height + gap) / (12.0 * rem + gap)).floor() as usize).max(1);

        Self {
            columns,
            tile_width,
            per_page: (columns * rows).clamp(1, MAX_AUTO_PAGE_SIZE),
        }
    }
}

/// Position of a pointer inside a pad waveform, as a fraction of the clip.
///
/// `None` keeps pad clicks toggling playback everywhere except on the
/// waveform row, where a click jumps to the matching point of the clip.
fn waveform_ratio(bounds: &Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    if !bounds.contains(&position) {
        return None;
    }
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return None;
    }
    Some(((f32::from(position.x) - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0))
}

/// Extra tolerance below the bars while the waveform is still growing.
///
/// The hovered row expands under the pointer; without a small margin the
/// pointer would fall outside the last painted bounds mid-animation and the
/// preview would flicker on and off.
const WAVEFORM_HOVER_SLOP: f32 = 4.0;

/// Position under the pointer inside the waveform seek zone.
///
/// This is [`waveform_ratio`] with a little room under the resting row so the
/// preview stays stable while the row grows toward the pointer. Hover and
/// click share it, so whatever shows a preview also seeks when clicked.
fn waveform_seek_ratio(bounds: &Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    let zone = Bounds {
        origin: bounds.origin,
        size: size(
            bounds.size.width,
            bounds.size.height + px(WAVEFORM_HOVER_SLOP),
        ),
    };
    waveform_ratio(&zone, position)
}

/// Whether the bar at `index` of `count` sits left of `ratio`.
///
/// Bars are compared by their centre so a fill covers the bar the pointer is
/// actually over instead of snapping to the bar's left edge.
fn waveform_bar_filled(index: usize, count: usize, ratio: f32) -> bool {
    if count == 0 {
        return false;
    }
    ((index as f32 + 0.5) / count as f32) <= ratio
}

pub struct SoundboardApp {
    sounds: Vec<Sound>,
    selected_id: Option<SoundId>,
    hovered_sound: Option<SoundId>,
    waveform_hover: Option<WaveformHover>,
    master_volume: f32,
    muted: bool,
    mic_monitor: bool,
    mic_gain: f32,
    looping: bool,
    /// Sounds are routed to the headphones instead of the virtual microphone.
    headphones_only: bool,
    /// Pads stack instead of interrupting the previous one.
    polyphonic: bool,

    paused: bool,
    transport: Option<TransportSnapshot>,
    playback_elapsed: f32,
    playback_generation: u64,
    last_telemetry: Instant,
    seek_dragging: bool,
    seek_preview: f32,

    output_level_l: f32,
    output_level_r: f32,
    mic_level: f32,
    /// Sounds with a voice on air, from the last telemetry tick.
    live_sounds: Vec<SoundId>,

    output_devices: Vec<DeviceChoice>,
    input_devices: Vec<DeviceChoice>,
    monitor_devices: Vec<DeviceChoice>,
    output_device: Option<String>,
    input_device: Option<String>,
    monitor_device: Option<String>,
    audio_error: Option<SharedString>,

    output_select: Entity<SelectState<Vec<SharedString>>>,
    input_select: Entity<SelectState<Vec<SharedString>>>,
    monitor_select: Entity<SelectState<Vec<SharedString>>>,
    master_slider: Entity<SliderState>,
    seek_slider: Entity<SliderState>,
    volume_sliders: HashMap<SoundId, Entity<SliderState>>,

    rename_target: Option<SoundId>,
    rename_input: Entity<InputState>,
    trim_target: Option<SoundId>,
    trim_start_pct: f32,
    trim_end_pct: f32,
    trim_drag: Option<TrimDrag>,
    trim_preview_active: bool,
    pending_trims: VecDeque<SoundId>,
    search_input: Entity<InputState>,
    search_query: String,
    page: usize,
    page_size: PageSize,
    page_size_select: Entity<SelectState<Vec<SharedString>>>,
    shortcut_target: Option<SoundId>,
    shortcut_error: Option<SharedString>,

    available_update: Option<AvailableUpdate>,
    update_downloading: bool,
    update_error: Option<SharedString>,
    update_dismissed: bool,

    /// The settings page and the release it is showing.
    settings_open: bool,
    settings_section: SettingsSection,
    latest_release: Option<update::Release>,
    checking_updates: bool,
    update_check_error: Option<SharedString>,

    audio: AudioEngine,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SoundboardApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let persisted = load_state();
        let output_devices = audio::list_output_devices();
        let input_devices = audio::list_input_devices();

        let mut audio = AudioEngine::new();
        let master_volume = persisted.master_volume.unwrap_or(0.8).clamp(0.0, 1.0);
        let mic_gain = persisted.mic_gain.unwrap_or(1.0).clamp(0.0, 2.0);
        let looping = persisted.looping.unwrap_or(false);
        let mic_monitor = persisted.mic_monitor.unwrap_or(false);
        audio.set_master_volume(master_volume);
        audio.set_mic_gain(mic_gain);

        let output_preference = persisted
            .output_device
            .as_deref()
            .filter(|name| output_devices.iter().any(|choice| choice.name == *name))
            .map(str::to_owned)
            .or_else(|| {
                output_devices
                    .iter()
                    .find(|choice| choice.is_default)
                    .or_else(|| output_devices.first())
                    .map(|choice| choice.name.clone())
            });
        let input_preference = persisted
            .input_device
            .as_deref()
            .filter(|name| input_devices.iter().any(|choice| choice.name == *name))
            .map(str::to_owned)
            .or_else(|| {
                input_devices
                    .iter()
                    .find(|choice| choice.is_default)
                    .or_else(|| input_devices.first())
                    .map(|choice| choice.name.clone())
            });

        let mut audio_error = None;
        let mut output_device = None;
        if let Some(name) = &output_preference {
            match audio.set_output_device(name) {
                Ok(_) => {
                    output_device = Some(name.clone());
                }
                Err(error) => {
                    audio_error = Some(error.to_string().into());
                }
            }
        } else {
            audio_error = Some("No audio output available".into());
        }

        let mut input_device = None;
        if let Some(name) = &input_preference {
            match audio.set_input_device(name) {
                Ok(_) => {
                    input_device = Some(name.clone());
                }
                Err(error) => {
                    if audio_error.is_none() {
                        audio_error = Some(error.to_string().into());
                    }
                }
            }
        }

        if mic_monitor && input_device.is_none() {
            audio.set_mic_monitor(false);
        }

        let monitor_devices = build_monitor_devices(&output_devices, output_device.as_deref());
        let main_output_is_virtual = output_device
            .as_ref()
            .and_then(|name| output_devices.iter().find(|choice| &choice.name == name))
            .is_some_and(|choice| choice.is_virtual);
        let monitor_preference =
            if output_device.is_none() || persisted.monitor_enabled == Some(false) {
                None
            } else if persisted.monitor_enabled == Some(true) {
                persisted
                    .monitor_device
                    .clone()
                    .filter(|name| monitor_devices.iter().any(|choice| &choice.name == name))
            } else if main_output_is_virtual {
                auto_monitor_device(&monitor_devices)
            } else {
                None
            };
        let mut monitor_device = None;
        if let Some(name) = monitor_preference {
            monitor_device = audio.set_monitor_device(&name).ok();
        }

        // Routing the bank into the headphones needs headphones to route into.
        let headphones_only =
            persisted.headphones_only.unwrap_or(false) && monitor_device.is_some();
        audio.set_headphones_only(headphones_only);

        let polyphonic = persisted.polyphonic.unwrap_or(false);
        audio.set_polyphonic(polyphonic);

        let output_labels = device_labels(&output_devices);
        let output_index = output_device
            .as_ref()
            .and_then(|name| {
                output_devices
                    .iter()
                    .position(|choice| choice.name == *name)
            })
            .map(|index| IndexPath::default().row(index));
        let output_select = cx.new(|cx| {
            SelectState::new(output_labels, output_index, window, cx)
                .searchable(output_devices.len() > 7)
        });

        let input_labels = device_labels(&input_devices);
        let input_index = input_device
            .as_ref()
            .and_then(|name| input_devices.iter().position(|choice| choice.name == *name))
            .map(|index| IndexPath::default().row(index));
        let input_select = cx.new(|cx| {
            SelectState::new(input_labels, input_index, window, cx)
                .searchable(input_devices.len() > 7)
        });

        let monitor_labels = monitor_device_labels(&monitor_devices);
        let monitor_index = monitor_device
            .as_ref()
            .and_then(|name| {
                monitor_devices
                    .iter()
                    .position(|choice| &choice.name == name)
            })
            .map(|index| 1 + index)
            .unwrap_or(0);
        let monitor_select = cx.new(|cx| {
            SelectState::new(
                monitor_labels,
                Some(IndexPath::default().row(monitor_index)),
                window,
                cx,
            )
        });

        let master_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(master_volume * 100.0)
        });
        let seek_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(0.1)
                .default_value(0.0)
        });
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Pad name"));
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search by title"));

        let mut sounds = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for saved in &persisted.sounds {
            let path = PathBuf::from(&saved.path);
            if !path.is_file() || !is_supported_audio(&path) {
                continue;
            }
            let path = normalize_path(&path);
            let id = sound_id(&path);
            if !seen.insert(id) {
                continue;
            }
            let mut sound = Sound::new(path, saved.volume);
            if let Some(title) = saved
                .title
                .as_ref()
                .filter(|title| !title.trim().is_empty())
            {
                sound.title = title.clone().into();
            }
            sound.trim_start = saved.trim_start.unwrap_or(0.0).max(0.0);
            sound.trim_end = saved.trim_end.unwrap_or(0.0).max(0.0);
            sound.shortcut = saved.shortcut.clone();
            sound.status = SoundStatus::Loading;
            sounds.push(sound);
        }

        let selected_id = persisted
            .selected_path
            .as_deref()
            .map(PathBuf::from)
            .map(|path| normalize_path(&path))
            .map(|path| sound_id(&path))
            .filter(|id| sounds.iter().any(|sound| sound.id == *id))
            .or_else(|| sounds.first().map(|sound| sound.id));
        let page_size = PageSize::from_saved(persisted.per_page);
        let page_size_select = cx.new(|cx| {
            SelectState::new(
                page_size_labels(),
                Some(IndexPath::default().row(page_size.index())),
                window,
                cx,
            )
        });
        let initial_per_page = match page_size {
            PageSize::Auto => LibraryMetrics::for_window(window).per_page,
            PageSize::Fixed(size) => size,
        };
        let initial_page = selected_id
            .and_then(|id| sounds.iter().position(|sound| sound.id == id))
            .map(|index| index / initial_per_page.max(1))
            .unwrap_or(0);

        let focus_handle = cx.focus_handle();
        let output_error = audio_error.clone();
        let mut app = Self {
            sounds,
            selected_id,
            hovered_sound: None,
            waveform_hover: None,
            master_volume,
            muted: false,
            mic_monitor: mic_monitor && input_device.is_some(),
            mic_gain,
            looping,
            headphones_only,
            polyphonic,
            paused: false,
            transport: None,
            playback_elapsed: 0.0,
            playback_generation: 0,
            last_telemetry: Instant::now(),
            seek_dragging: false,
            seek_preview: 0.0,
            output_level_l: 0.0,
            output_level_r: 0.0,
            mic_level: 0.0,
            live_sounds: Vec::new(),
            output_devices,
            input_devices,
            monitor_devices,
            output_device,
            input_device,
            monitor_device,
            audio_error: output_error,
            output_select,
            input_select,
            monitor_select,
            master_slider,
            seek_slider,
            volume_sliders: HashMap::new(),
            rename_target: None,
            rename_input,
            trim_target: None,
            trim_start_pct: 0.0,
            trim_end_pct: 100.0,
            trim_drag: None,
            trim_preview_active: false,
            pending_trims: VecDeque::new(),
            search_input,
            search_query: String::new(),
            page: initial_page,
            page_size,
            page_size_select,
            shortcut_target: None,
            shortcut_error: None,
            available_update: None,
            update_downloading: false,
            update_error: None,
            update_dismissed: false,
            settings_open: false,
            settings_section: SettingsSection::default(),
            latest_release: None,
            checking_updates: false,
            update_check_error: None,
            audio,
            focus_handle,
            _subscriptions: Vec::new(),
        };

        app.audio.set_mic_monitor(app.mic_monitor);
        app.audio_error = match (&app.output_device, app.audio_error.take()) {
            (Some(_), _) => None,
            (None, Some(error)) => Some(error),
            (None, None) => None,
        };

        let subscriptions = vec![
            cx.subscribe_in(
                &app.output_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    if let Some(index) = this
                        .output_devices
                        .iter()
                        .position(|choice| choice.label() == label.as_ref())
                    {
                        this.choose_output(index, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &app.input_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    if let Some(index) = this
                        .input_devices
                        .iter()
                        .position(|choice| choice.label() == label.as_ref())
                    {
                        this.choose_input(index, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &app.master_slider,
                window,
                |this, _, event: &SliderEvent, window, cx| match event {
                    SliderEvent::Change(value) => {
                        this.apply_master_volume(value.start() / 100.0, window, cx);
                    }
                    SliderEvent::Release(value) => {
                        this.apply_master_volume(value.start() / 100.0, window, cx);
                        this.save_state();
                    }
                },
            ),
            cx.subscribe_in(
                &app.seek_slider,
                window,
                |this, _, event: &SliderEvent, _window, cx| match event {
                    SliderEvent::Change(value) => {
                        this.seek_dragging = true;
                        this.seek_preview = value.start() / 100.0;
                        cx.notify();
                    }
                    SliderEvent::Release(value) => {
                        this.seek_dragging = false;
                        this.audio.seek_ratio(value.start() / 100.0);
                        this.seek_preview = value.start() / 100.0;
                        let duration = this
                            .transport
                            .as_ref()
                            .map(|current| current.duration)
                            .unwrap_or(0.0);
                        this.playback_elapsed = duration * value.start() / 100.0;
                        this.last_telemetry = Instant::now();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &app.monitor_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    let index = if label.as_ref() == "None" {
                        0
                    } else {
                        this.monitor_devices
                            .iter()
                            .position(|choice| choice.label() == label.as_ref())
                            .map(|index| index + 1)
                            .unwrap_or(0)
                    };
                    this.choose_monitor(index, window, cx);
                },
            ),
            cx.subscribe_in(
                &app.rename_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(
                        event,
                        InputEvent::PressEnter {
                            secondary: false,
                            shift: false
                        }
                    ) {
                        this.commit_rename(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &app.search_input,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.search_query = this.search_input.read(cx).value().to_string();
                        this.page = 0;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &app.page_size_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    if let Some(size) = PageSize::from_label(label.as_ref()) {
                        this.change_page_size(size, window, cx);
                    }
                },
            ),
        ];
        app._subscriptions = subscriptions;

        let initial_volumes: Vec<(SoundId, f32)> = app
            .sounds
            .iter()
            .map(|sound| (sound.id, sound.volume))
            .collect();
        for (id, volume) in initial_volumes {
            app.attach_volume_slider(id, volume, window, cx);
        }

        if !app.sounds.is_empty() {
            let paths: Vec<(SoundId, PathBuf)> = app
                .sounds
                .iter()
                .map(|sound| (sound.id, sound.path.clone()))
                .collect();
            for (id, path) in paths {
                app.load_sound(id, path, false, window, cx);
            }
        }

        cx.defer_in(window, |this, window, cx| {
            window.focus(&this.focus_handle, cx);
        });

        if std::env::var_os("SIMPLE_SOUNDBOARD_SKIP_UPDATE_CHECK").is_none() {
            cx.spawn_in(window, async move |this, window| {
                let result = window.background_spawn(async { update::check() }).await;
                match result {
                    Ok(Some(available)) => {
                        let _ = this.update_in(window, |this, _, cx| {
                            this.available_update = Some(available);
                            this.update_dismissed = false;
                            cx.notify();
                        });
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("Update check failed: {error}"),
                }
            })
            .detach();
        }

        cx.spawn_in(window, async move |this, window| {
            let mut consecutive_errors = 0u32;
            loop {
                Timer::after(Duration::from_millis(33)).await;
                let result = this.update_in(window, |this, window, cx| {
                    this.refresh_telemetry(window, cx);
                });
                if result.is_ok() {
                    consecutive_errors = 0;
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= 20 {
                    eprintln!("UI telemetry stopped: the window can no longer be updated.");
                    break;
                }
                Timer::after(Duration::from_millis(200)).await;
            }
        })
        .detach();

        app
    }

    fn load_sound(
        &mut self,
        id: SoundId,
        path: PathBuf,
        trim_after_load: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, window| {
            let result = window
                .background_spawn(async move { audio::decode_audio(&path) })
                .await;
            let _ = this.update_in(window, |this, window, cx| {
                let Some(sound) = this.sounds.iter_mut().find(|sound| sound.id == id) else {
                    cx.notify();
                    return;
                };
                match result {
                    Ok(decoded) => {
                        sound.set_source(Arc::new(decoded));
                        if trim_after_load {
                            this.pending_trims.push_back(id);
                        }
                    }
                    Err(error) => sound.set_failed(error.to_string()),
                }
                if this.selected_id.is_none() {
                    this.selected_id = Some(id);
                }
                this.maybe_open_next_trim(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn attach_volume_slider(
        &mut self,
        id: SoundId,
        volume: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.volume_sliders.contains_key(&id) {
            return;
        }
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(volume.clamp(0.0, 1.0) * 100.0)
        });
        let subscription = cx.subscribe_in(
            &slider,
            window,
            move |this, _, event: &SliderEvent, _window, cx| match event {
                SliderEvent::Change(value) => {
                    this.apply_sound_volume_value(id, value.start() / 100.0, false, cx);
                }
                SliderEvent::Release(value) => {
                    this.apply_sound_volume_value(id, value.start() / 100.0, true, cx);
                }
            },
        );
        self.volume_sliders.insert(id, slider);
        self._subscriptions.push(subscription);
    }

    fn apply_sound_volume_value(
        &mut self,
        id: SoundId,
        volume: f32,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(sound) = self.sounds.iter_mut().find(|sound| sound.id == id) else {
            return;
        };
        sound.volume = volume.clamp(0.0, 1.0);
        self.audio.set_sound_volume(id, sound.volume);
        if persist {
            self.save_state();
        }
        cx.notify();
    }

    fn maybe_open_next_trim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trim_target.is_some() || self.rename_target.is_some() {
            return;
        }
        while let Some(id) = self.pending_trims.pop_front() {
            if self
                .sounds
                .iter()
                .any(|sound| sound.id == id && sound.source.is_some())
            {
                self.open_trim_for(id, window, cx);
                break;
            }
        }
    }

    fn open_trim_for(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((duration, trim_start, trim_end)) = self
            .sounds
            .iter()
            .find(|sound| sound.id == id)
            .map(|sound| (sound.full_duration(), sound.trim_start, sound.trim_end))
        else {
            return;
        };
        if duration <= 0.0 {
            return;
        }
        if self.trim_preview_active {
            self.stop_playback(window, cx);
        }
        let min_gap = (0.05 / duration * 100.0).clamp(0.05, 99.0);
        let start = (trim_start / duration * 100.0).clamp(0.0, 100.0 - min_gap);
        let end = (trim_end / duration * 100.0).clamp(start + min_gap, 100.0);
        self.selected_id = Some(id);
        self.trim_target = Some(id);
        self.trim_start_pct = start;
        self.trim_end_pct = end;
        self.trim_drag = None;
        cx.notify();
    }

    fn cancel_trim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trim_preview_active {
            self.stop_playback(window, cx);
        }
        if self.trim_target.take().is_none() {
            return;
        }
        self.trim_drag = None;
        self.maybe_open_next_trim(window, cx);
        cx.notify();
    }

    fn trim_min_gap_pct(&self) -> f32 {
        let duration = self
            .trim_target
            .and_then(|id| self.sounds.iter().find(|sound| sound.id == id))
            .map(|sound| sound.full_duration())
            .unwrap_or(1.0)
            .max(0.01);
        (0.05 / duration * 100.0).clamp(0.05, 99.0)
    }

    fn begin_trim_drag(
        &mut self,
        kind: TrimDrag,
        pointer: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.trim_target.is_none() {
            return;
        }
        if self.trim_preview_active {
            self.stop_playback(window, cx);
        }
        let pointer = pointer.clamp(0.0, 100.0);
        let min_gap = self.trim_min_gap_pct();
        match kind {
            TrimDrag::Start => {
                let max_start = (self.trim_end_pct - min_gap).max(0.0);
                self.trim_start_pct = pointer.clamp(0.0, max_start);
            }
            TrimDrag::End => {
                let min_end = (self.trim_start_pct + min_gap).min(100.0);
                self.trim_end_pct = pointer.clamp(min_end, 100.0);
            }
            TrimDrag::Window { .. } => {}
        }
        self.trim_drag = Some(kind);
        cx.notify();
    }

    fn update_trim_drag(&mut self, pointer: f32, cx: &mut Context<Self>) {
        let Some(kind) = self.trim_drag else {
            return;
        };
        let pointer = pointer.clamp(0.0, 100.0);
        let min_gap = self.trim_min_gap_pct();
        match kind {
            TrimDrag::Start => {
                let max_start = (self.trim_end_pct - min_gap).max(0.0);
                self.trim_start_pct = pointer.clamp(0.0, max_start);
            }
            TrimDrag::End => {
                let min_end = (self.trim_start_pct + min_gap).min(100.0);
                self.trim_end_pct = pointer.clamp(min_end, 100.0);
            }
            TrimDrag::Window { grab } => {
                let span = (self.trim_end_pct - self.trim_start_pct).max(min_gap);
                let start = (pointer - grab).clamp(0.0, 100.0 - span);
                self.trim_start_pct = start;
                self.trim_end_pct = start + span;
            }
        }
        cx.notify();
    }

    fn end_trim_drag(&mut self, cx: &mut Context<Self>) {
        if self.trim_drag.take().is_some() {
            cx.notify();
        }
    }

    fn toggle_trim_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trim_preview_active {
            self.stop_playback(window, cx);
            return;
        }
        let Some(id) = self.trim_target else {
            return;
        };
        let Some(sound) = self.sounds.iter().find(|sound| sound.id == id) else {
            return;
        };
        let Some(source) = sound.source.clone() else {
            return;
        };
        let volume = sound.volume;
        let duration = source.duration;
        let start = duration * self.trim_start_pct / 100.0;
        let end = duration * self.trim_end_pct / 100.0;
        if end - start < 0.05 {
            window.push_notification("The selected portion is too short to audition.", cx);
            return;
        }
        let preview_duration = end - start;
        let preview = source.trimmed(start, end);
        self.audio
            .trigger(id, Arc::new(preview), volume, false, 0.0);
        self.selected_id = Some(id);
        self.paused = false;
        self.trim_preview_active = true;
        self.playback_elapsed = 0.0;
        self.playback_generation = self.playback_generation.wrapping_add(1);
        let generation = self.playback_generation;
        self.last_telemetry = Instant::now();
        self.seek_dragging = false;
        self.seek_preview = 0.0;
        self.transport = self.audio.snapshot().current;
        self.seek_slider.update(cx, |slider, cx| {
            slider.set_value(0.0, window, cx);
        });
        self.schedule_playback_fallback(preview_duration, generation, window, cx);
        cx.notify();
    }

    fn apply_trim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.trim_target else {
            return;
        };
        let Some(sound) = self.sounds.iter().find(|sound| sound.id == id) else {
            self.trim_target = None;
            return;
        };
        let duration = sound.full_duration();
        if duration <= 0.0 {
            return;
        }
        let start = duration * self.trim_start_pct / 100.0;
        let end = duration * self.trim_end_pct / 100.0;
        if end - start < 0.05 {
            window.push_notification("The trim must last at least 0.05 seconds.", cx);
            return;
        }

        let applied = self
            .sounds
            .iter_mut()
            .find(|sound| sound.id == id)
            .is_some_and(|sound| sound.apply_trim(start, end));
        if !applied {
            window.push_notification("Could not apply the trim.", cx);
            return;
        }

        self.audio.stop_sound(id);
        self.trim_preview_active = false;
        if self
            .transport
            .as_ref()
            .is_some_and(|current| current.sound_id == id)
        {
            self.transport = None;
            self.paused = false;
            self.playback_elapsed = 0.0;
            self.seek_dragging = false;
            self.seek_slider.update(cx, |slider, cx| {
                slider.set_value(0.0, window, cx);
            });
        }
        self.trim_target = None;
        self.trim_drag = None;
        self.save_state();
        self.maybe_open_next_trim(window, cx);
        window.push_notification("Trim applied.", cx);
        cx.notify();
    }

    fn begin_rename(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        if self.trim_target.is_some() {
            return;
        }
        let Some(title) = self
            .sounds
            .iter()
            .find(|sound| sound.id == id)
            .map(|sound| sound.title.clone())
        else {
            return;
        };
        self.selected_id = Some(id);
        self.rename_target = Some(id);
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.rename_target else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_string();
        if title.is_empty() {
            window.push_notification("The pad name cannot be empty.", cx);
            return;
        }
        if let Some(sound) = self.sounds.iter_mut().find(|sound| sound.id == id) {
            sound.title = title.into();
        } else {
            self.rename_target = None;
            return;
        }
        self.rename_target = None;
        self.save_state();
        window.focus(&self.focus_handle, cx);
        self.maybe_open_next_trim(window, cx);
        window.push_notification("Pad renamed.", cx);
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename_target = None;
        window.focus(&self.focus_handle, cx);
        self.maybe_open_next_trim(window, cx);
        cx.notify();
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.search_input.update(cx, |input, cx| {
            input.focus(window, cx);
        });
    }

    fn begin_shortcut_capture(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcut_target = Some(id);
        self.shortcut_error = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel_shortcut_capture(&mut self, cx: &mut Context<Self>) {
        self.shortcut_target = None;
        self.shortcut_error = None;
        cx.notify();
    }

    fn clear_shortcut(&mut self, id: SoundId, cx: &mut Context<Self>) {
        if let Some(sound) = self.sounds.iter_mut().find(|sound| sound.id == id) {
            sound.shortcut = None;
        }
        if self.shortcut_target == Some(id) {
            self.shortcut_target = None;
            self.shortcut_error = None;
        }
        self.save_state();
        cx.notify();
    }

    fn on_shortcut_key(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The settings page is a separate screen: the escape action leaves it
        // (see on_stop_action) and pad shortcuts stay quiet while it is open.
        if self.settings_open {
            cx.stop_propagation();
            return;
        }

        if let Some(id) = self.shortcut_target {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                self.cancel_shortcut_capture(cx);
                cx.stop_propagation();
                return;
            }
            if event.is_held
                || crate::model::is_modifier_key(&event.keystroke.key.to_ascii_lowercase())
            {
                cx.stop_propagation();
                return;
            }

            let Some(shortcut) = KeyShortcut::from_keystroke(&event.keystroke) else {
                self.shortcut_error = Some(
                    "That shortcut is reserved or not valid. Try Ctrl/Alt + key or an F-key."
                        .into(),
                );
                cx.notify();
                cx.stop_propagation();
                return;
            };

            if let Some(other) = self
                .sounds
                .iter()
                .find(|sound| sound.id != id && sound.shortcut.as_ref() == Some(&shortcut))
            {
                self.shortcut_error = Some(format!("Already used by “{}”.", other.title).into());
                cx.notify();
                cx.stop_propagation();
                return;
            }

            if let Some(sound) = self.sounds.iter_mut().find(|sound| sound.id == id) {
                sound.shortcut = Some(shortcut);
            }
            self.shortcut_target = None;
            self.shortcut_error = None;
            self.save_state();
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if event.is_held {
            return;
        }
        let Some(shortcut) = KeyShortcut::from_keystroke(&event.keystroke) else {
            return;
        };
        let Some(id) = self
            .sounds
            .iter()
            .find(|sound| sound.shortcut.as_ref() == Some(&shortcut))
            .map(|sound| sound.id)
        else {
            return;
        };

        self.reveal_sound(id, self.per_page(window));
        self.activate_sound(id, window, cx);
        cx.stop_propagation();
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.search_query.clear();
        self.page = 0;
        cx.notify();
    }

    fn filtered_sound_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.sounds.len()).collect();
        }
        self.sounds
            .iter()
            .enumerate()
            .filter(|(_, sound)| sound.title.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    /// Sounds shown on one page: the selected fixed size or the window fit.
    fn per_page(&self, window: &Window) -> usize {
        match self.page_size {
            PageSize::Auto => LibraryMetrics::for_window(window).per_page,
            PageSize::Fixed(size) => size,
        }
    }

    fn page_count(&self, per_page: usize) -> usize {
        self.filtered_sound_indices()
            .len()
            .div_ceil(per_page.max(1))
            .max(1)
    }

    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.page -= 1;
            cx.notify();
        }
    }

    fn next_page(&mut self, per_page: usize, cx: &mut Context<Self>) {
        if self.page + 1 < self.page_count(per_page) {
            self.page += 1;
            cx.notify();
        }
    }

    fn clamp_page(&mut self, per_page: usize) {
        self.page = self.page.min(self.page_count(per_page) - 1);
    }

    fn reveal_sound(&mut self, id: SoundId, per_page: usize) {
        let filtered = self.filtered_sound_indices();
        if let Some(position) = filtered
            .iter()
            .position(|&index| self.sounds[index].id == id)
        {
            self.page = position / per_page.max(1);
        }
    }

    /// Switch page size, keeping the page that holds the first visible pad.
    fn change_page_size(&mut self, size: PageSize, window: &mut Window, cx: &mut Context<Self>) {
        if size == self.page_size {
            return;
        }
        let first_visible = self.page.saturating_mul(self.per_page(window));
        self.page_size = size;
        self.page = first_visible / self.per_page(window).max(1);
        self.save_state();
        cx.notify();
    }

    fn choose_monitor(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = if index == 0 {
            None
        } else {
            self.monitor_devices
                .get(index - 1)
                .map(|choice| choice.name.clone())
        };

        if let Some(device_name) = name {
            if self.output_device.as_deref() == Some(device_name.as_str()) {
                window.push_notification(
                    "The headphone monitor must differ from the main output.",
                    cx,
                );
                self.restore_monitor_selection(window, cx);
                return;
            }
            match self.audio.set_monitor_device(&device_name) {
                Ok(name) => {
                    self.monitor_device = Some(name);
                    self.save_state();
                    window.push_notification("Headphone monitor on.", cx);
                    cx.notify();
                }
                Err(error) => {
                    self.audio.clear_monitor_device();
                    self.monitor_device = None;
                    self.drop_headphones_only();
                    self.restore_monitor_selection(window, cx);
                    self.save_state();
                    window.push_notification(
                        format!("Could not open the headphone monitor: {error}"),
                        cx,
                    );
                    cx.notify();
                }
            }
        } else {
            self.audio.clear_monitor_device();
            self.monitor_device = None;
            self.drop_headphones_only();
            self.save_state();
            window.push_notification("Headphone monitor off.", cx);
            cx.notify();
        }
    }

    fn sync_monitor_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut devices =
            build_monitor_devices(&self.output_devices, self.output_device.as_deref());
        devices.sort_by(|a, b| {
            (a.is_virtual, !a.is_default, &a.name).cmp(&(b.is_virtual, !b.is_default, &b.name))
        });
        self.monitor_devices = devices;

        if self.monitor_device.as_ref().is_some_and(|name| {
            !self
                .monitor_devices
                .iter()
                .any(|choice| &choice.name == name)
        }) {
            self.audio.clear_monitor_device();
            self.monitor_device = None;
            self.drop_headphones_only();
        }

        let labels = monitor_device_labels(&self.monitor_devices);
        let selected = self
            .monitor_device
            .as_ref()
            .and_then(|name| {
                self.monitor_devices
                    .iter()
                    .position(|choice| &choice.name == name)
            })
            .map(|index| IndexPath::default().row(index + 1))
            .unwrap_or_default();
        self.monitor_select.update(cx, |state, cx| {
            state.set_items(labels, window, cx);
            state.set_selected_index(Some(selected), window, cx);
        });
    }

    fn restore_monitor_selection(&self, window: &mut Window, cx: &mut App) {
        let selected = self
            .monitor_device
            .as_ref()
            .and_then(|name| {
                self.monitor_devices
                    .iter()
                    .position(|choice| &choice.name == name)
            })
            .map(|index| IndexPath::default().row(index + 1))
            .unwrap_or_default();
        self.monitor_select.update(cx, |state, cx| {
            state.set_selected_index(Some(selected), window, cx);
        });
    }

    fn refresh_telemetry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.audio.snapshot();
        let old_l = self.output_level_l;
        let old_r = self.output_level_r;
        let old_mic = self.mic_level;
        let old_current = self.transport.clone();
        let old_paused = self.paused;

        let now = Instant::now();
        let delta = now
            .duration_since(self.last_telemetry)
            .as_secs_f32()
            .clamp(0.0, 0.5);
        self.last_telemetry = now;

        self.output_level_l = snapshot.output_level_l;
        self.output_level_r = snapshot.output_level_r;
        self.mic_level = snapshot.mic_level;
        self.transport = snapshot.current.clone();
        self.paused = snapshot.paused;
        self.live_sounds = snapshot.playing;

        let mut changed = false;

        let previous_id = old_current.as_ref().map(|current| current.sound_id);
        let next_id = self.transport.as_ref().map(|current| current.sound_id);
        if previous_id != next_id {
            changed = true;
            // A promoted voice starts its own clock: in polyphonic mode the
            // transport moves to another pad when the current one ends.
            self.playback_elapsed = 0.0;
            self.seek_dragging = false;
            self.seek_preview = 0.0;
            if next_id.is_none() {
                self.trim_preview_active = false;
                let seek = self.seek_slider.clone();
                seek.update(cx, |slider, cx| slider.set_value(0.0, window, cx));
            } else if let Some(id) = next_id {
                // With pads stacking the bar follows the pad that is still on
                // air, unless the user is busy renaming or trimming.
                if self.rename_target.is_none() && self.trim_target.is_none() {
                    self.selected_id = Some(id);
                }
            }
        }

        let loop_allowed = self.looping && !self.trim_preview_active;
        if self.transport.is_some() && !self.paused && !loop_allowed {
            self.playback_elapsed += delta;
        } else if self.transport.is_none() || loop_allowed {
            self.playback_elapsed = 0.0;
        }

        let reached_end = self.transport.as_ref().is_some_and(|current| {
            current.duration > 0.0
                && (current.position + 0.0005 >= current.duration
                    || playback_watchdog_expired(self.playback_elapsed, current.duration))
        });
        // With pads stacking, other voices stay on air when one ends: the
        // mixer promotes the next voice and the transport follows it.
        if reached_end && !self.paused && !loop_allowed && !self.polyphonic {
            self.audio.stop_all();
            self.transport = None;
            self.paused = false;
            self.trim_preview_active = false;
            self.playback_elapsed = 0.0;
        }

        if (self.output_level_l - old_l).abs() > 0.001
            || (self.output_level_r - old_r).abs() > 0.001
            || (self.mic_level - old_mic).abs() > 0.001
        {
            changed = true;
        }
        if old_paused != self.paused {
            changed = true;
        }

        if let Some(current) = &self.transport {
            if !self.seek_dragging {
                let ratio = if current.duration > 0.0 {
                    (current.position / current.duration).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let existing = self.seek_slider.read(cx).value().start();
                if (existing - ratio * 100.0).abs() > 0.4 {
                    let seek = self.seek_slider.clone();
                    seek.update(cx, |slider, cx| slider.set_value(ratio * 100.0, window, cx));
                }
            }
            if let Some(previous) = &old_current {
                if (current.position - previous.position).abs() > 0.02 {
                    changed = true;
                }
            } else {
                changed = true;
            }
        }

        if changed {
            cx.notify();
        }
    }

    fn selected_sound(&self) -> Option<&Sound> {
        let id = self.selected_id?;
        self.sounds.iter().find(|sound| sound.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id?;
        self.sounds.iter().position(|sound| sound.id == id)
    }

    fn is_playing(&self) -> bool {
        self.transport.is_some() && !self.paused
    }

    fn is_sound_playing(&self, id: SoundId) -> bool {
        !self.paused && self.live_sounds.contains(&id)
    }

    fn output_is_virtual(&self) -> bool {
        self.output_device
            .as_ref()
            .and_then(|name| {
                self.output_devices
                    .iter()
                    .find(|choice| choice.name == *name)
            })
            .is_some_and(|choice| choice.is_virtual)
    }

    fn choose_output(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self.output_devices.get(index).cloned() else {
            return;
        };
        match self.audio.set_output_device(&choice.name) {
            Ok(name) => {
                if self.monitor_device.as_deref() == Some(name.as_str()) {
                    self.audio.clear_monitor_device();
                    self.monitor_device = None;
                    self.drop_headphones_only();
                }
                self.output_device = Some(name);
                self.sync_monitor_devices(window, cx);
                self.audio_error = None;
                self.save_state();
                window.push_notification("Audio output updated.", cx);
                cx.notify();
            }
            Err(error) => {
                self.audio_error = Some(error.to_string().into());
                window.push_notification(format!("Could not open {}: {error}", choice.name), cx);
                self.restore_output_selection(window, cx);
                cx.notify();
            }
        }
    }

    fn choose_input(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self.input_devices.get(index).cloned() else {
            return;
        };
        match self.audio.set_input_device(&choice.name) {
            Ok(name) => {
                self.input_device = Some(name);
                self.save_state();
                window.push_notification("Microphone updated.", cx);
                cx.notify();
            }
            Err(error) => {
                window.push_notification(format!("Could not open {}: {error}", choice.name), cx);
                self.restore_input_selection(window, cx);
                cx.notify();
            }
        }
    }

    fn restore_output_selection(&self, window: &mut Window, cx: &mut App) {
        let selected = self
            .output_device
            .as_ref()
            .and_then(|name| {
                self.output_devices
                    .iter()
                    .position(|choice| choice.name == *name)
            })
            .map(|index| IndexPath::default().row(index));
        self.output_select.update(cx, |state, cx| {
            state.set_selected_index(selected, window, cx);
        });
    }

    fn restore_input_selection(&self, window: &mut Window, cx: &mut App) {
        let selected = self
            .input_device
            .as_ref()
            .and_then(|name| {
                self.input_devices
                    .iter()
                    .position(|choice| choice.name == *name)
            })
            .map(|index| IndexPath::default().row(index));
        self.input_select.update(cx, |state, cx| {
            state.set_selected_index(selected, window, cx);
        });
    }

    fn play_sound(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        self.play_sound_from(id, 0.0, window, cx);
    }

    /// Start a sound, optionally from a fraction of its trimmed duration.
    fn play_sound_from(
        &mut self,
        id: SoundId,
        start_ratio: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.output_device.is_none() {
            window.push_notification("Choose an audio output before going on air.", cx);
            return;
        }
        let Some(sound) = self.sounds.iter().find(|sound| sound.id == id) else {
            return;
        };
        let Some(decoded) = sound.decoded.clone() else {
            let message = match &sound.status {
                SoundStatus::Loading => "The file is still loading.".to_string(),
                SoundStatus::Failed(error) => error.to_string(),
                SoundStatus::Ready => "The file is unavailable.".to_string(),
            };
            window.push_notification(message, cx);
            return;
        };

        let start_ratio = start_ratio.clamp(0.0, 1.0);
        let duration = decoded.duration;
        self.selected_id = Some(id);
        self.trim_preview_active = false;
        self.audio
            .trigger(id, decoded, sound.volume, self.looping, start_ratio);
        self.paused = false;
        self.playback_elapsed = 0.0;
        self.playback_generation = self.playback_generation.wrapping_add(1);
        let generation = self.playback_generation;
        self.last_telemetry = Instant::now();
        self.seek_dragging = false;
        self.seek_preview = 0.0;
        self.transport = self.audio.snapshot().current;
        self.seek_slider.update(cx, |slider, cx| {
            slider.set_value(start_ratio * 100.0, window, cx);
        });
        self.schedule_playback_fallback(duration, generation, window, cx);
        self.save_state();
        cx.notify();
    }

    /// Jump to a point of a pad's waveform: seek the clip already on air, or
    /// start it from there.
    fn seek_from_waveform(
        &mut self,
        id: SoundId,
        ratio: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let can_seek = self
            .sounds
            .iter()
            .any(|sound| sound.id == id && sound.is_ready() && sound.duration() > 0.0);
        if !can_seek {
            return;
        }

        let ratio = ratio.clamp(0.0, 1.0);
        let is_current = self
            .transport
            .as_ref()
            .is_some_and(|current| current.sound_id == id);
        if !is_current {
            self.play_sound_from(id, ratio, window, cx);
            return;
        }

        let selection_changed = self.selected_id != Some(id);
        self.selected_id = Some(id);
        self.audio.seek_ratio(ratio);
        self.paused = false;
        self.audio.set_paused(false);
        self.playback_elapsed = 0.0;
        self.last_telemetry = Instant::now();
        self.seek_dragging = false;
        self.transport = self.audio.snapshot().current;
        self.seek_slider.update(cx, |slider, cx| {
            slider.set_value(ratio * 100.0, window, cx);
        });
        if selection_changed {
            self.save_state();
        }
        cx.notify();
    }

    /// Track the pointer over a pad waveform so the pad can preview the seek
    /// before the click commits it.
    fn preview_waveform_hover(&mut self, id: SoundId, ratio: Option<f32>, cx: &mut Context<Self>) {
        let next = ratio.map(|ratio| WaveformHover {
            sound_id: id,
            ratio,
        });
        let changed = match (self.waveform_hover, next) {
            (Some(current), Some(next)) => {
                current.sound_id != next.sound_id
                    // Sub-pixel moves are not worth a repaint.
                    || (current.ratio - next.ratio).abs() > 0.002
            }
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.waveform_hover = next;
            cx.notify();
        }
    }

    /// Drop the waveform preview, if any.
    fn clear_waveform_hover(&mut self, cx: &mut Context<Self>) {
        if self.waveform_hover.take().is_some() {
            cx.notify();
        }
    }

    fn schedule_playback_fallback(
        &self,
        duration: f32,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if duration <= 0.0 {
            return;
        }
        cx.spawn_in(window, async move |this, window| {
            Timer::after(Duration::from_secs_f32(duration + 0.3)).await;
            let _ = this.update_in(window, |this, window, cx| {
                if this.playback_generation != generation {
                    return;
                }
                let Some(current) = this.transport.as_ref() else {
                    return;
                };
                if this.paused || (this.looping && !this.trim_preview_active) {
                    return;
                }
                if current.position > 0.01 {
                    return;
                }
                eprintln!("Transport fallback: the UI did not update playback; resetting state.");
                this.stop_playback(window, cx);
            });
        })
        .detach();
    }

    /// Let pads play on top of each other: a trigger no longer releases the
    /// previous clip.
    fn toggle_polyphonic(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.polyphonic = enabled;
        self.audio.set_polyphonic(enabled);
        self.save_state();
        cx.notify();
    }

    fn activate_sound(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        let is_current = self
            .transport
            .as_ref()
            .is_some_and(|current| current.sound_id == id);
        if is_current {
            if self.paused {
                self.paused = false;
                self.audio.set_paused(false);
                self.last_telemetry = Instant::now();
                cx.notify();
            } else if self.polyphonic {
                // Other pads may still be on air: stop this one only.
                self.audio.stop_sound(id);
            } else {
                self.stop_playback(window, cx);
            }
            return;
        }
        self.play_sound(id, window, cx);
    }

    fn toggle_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transport.is_some() {
            self.paused = !self.paused;
            self.audio.set_paused(self.paused);
            cx.notify();
            return;
        }
        if let Some(id) = self.selected_id.or_else(|| {
            self.sounds
                .iter()
                .find(|sound| sound.is_ready())
                .map(|sound| sound.id)
        }) {
            self.play_sound(id, window, cx);
        }
    }

    fn stop_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.audio.stop_all();
        self.playback_generation = self.playback_generation.wrapping_add(1);
        self.trim_preview_active = false;
        self.paused = false;
        self.transport = None;
        self.playback_elapsed = 0.0;
        self.last_telemetry = Instant::now();
        self.seek_dragging = false;
        self.seek_preview = 0.0;
        self.seek_slider.update(cx, |slider, cx| {
            slider.set_value(0.0, window, cx);
        });
        cx.notify();
    }

    fn skip(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.sounds.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let length = self.sounds.len() as isize;
        let next = ((current as isize + delta).rem_euclid(length)) as usize;
        let was_playing = self.is_playing();
        let id = self.sounds[next].id;
        self.selected_id = Some(id);
        self.reveal_sound(id, self.per_page(window));
        if was_playing {
            self.play_sound(id, window, cx);
        } else {
            self.audio.stop_all();
            self.trim_preview_active = false;
            self.transport = None;
            self.paused = false;
            self.seek_dragging = false;
            self.seek_preview = 0.0;
            self.seek_slider.update(cx, |slider, cx| {
                slider.set_value(0.0, window, cx);
            });
            self.save_state();
            cx.notify();
        }
    }

    fn previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .transport
            .as_ref()
            .is_some_and(|current| current.position > 3.0)
        {
            self.audio.seek_ratio(0.0);
            self.seek_slider.update(cx, |slider, cx| {
                slider.set_value(0.0, window, cx);
            });
            cx.notify();
            return;
        }
        self.skip(-1, window, cx);
    }

    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        self.looping = !self.looping;
        self.audio.set_current_looping(self.looping);
        self.save_state();
        cx.notify();
    }

    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        self.audio
            .set_master_volume(if self.muted { 0.0 } else { self.master_volume });
        cx.notify();
    }

    fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        self.update_dismissed = true;
        cx.notify();
    }

    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            self.close_settings(cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        cx.notify();
        if self.latest_release.is_none() {
            self.check_updates(window, cx);
        }
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    /// Show a different category in the settings page.
    fn choose_settings_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.settings_section == section {
            return;
        }
        self.settings_section = section;
        cx.notify();
    }

    /// Fetch the latest release for the settings page. The running build keeps
    /// playing: only the update cards change.
    fn check_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.checking_updates {
            return;
        }
        self.checking_updates = true;
        self.update_check_error = None;
        self.update_error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, window| {
            let result = window
                .background_spawn(async { update::fetch_latest() })
                .await;
            let _ = this.update_in(window, |this, _, cx| {
                this.checking_updates = false;
                match result {
                    Ok(Some(release)) => {
                        this.available_update = release.update();
                        this.latest_release = Some(release);
                    }
                    Ok(None) => {
                        this.available_update = None;
                        this.latest_release = None;
                    }
                    Err(error) => this.update_check_error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The toast's "See changes" lands in the settings page, where the notes
    /// are rendered, instead of throwing the user at a browser tab.
    fn show_release_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_dismissed = true;
        self.settings_section = SettingsSection::About;
        self.open_settings(window, cx);
    }

    /// Open the release the settings page is showing, falling back to the
    /// update offered by the startup check.
    fn open_release_page(&mut self, cx: &mut Context<Self>) {
        if let Some(release) = &self.latest_release {
            cx.open_url(&release.release_url);
        } else if let Some(available) = &self.available_update {
            cx.open_url(&available.release_url);
        }
    }

    fn install_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_downloading {
            return;
        }
        let Some(available) = self.available_update.clone() else {
            return;
        };

        self.update_downloading = true;
        self.update_error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, window| {
            let result = window
                .background_spawn(async move { update::download(&available) })
                .await;
            match result {
                Ok(setup) => match update::run_installer(&setup) {
                    Ok(()) => {
                        let _ = this.update_in(window, |this, _, cx| {
                            this.update_downloading = false;
                            cx.quit();
                        });
                    }
                    Err(error) => {
                        let _ = this.update_in(window, |this, _, cx| {
                            this.update_downloading = false;
                            this.update_error = Some(error.into());
                            cx.notify();
                        });
                    }
                },
                Err(error) => {
                    let _ = this.update_in(window, |this, _, cx| {
                        this.update_downloading = false;
                        this.update_error = Some(error.into());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn apply_master_volume(&mut self, volume: f32, _window: &mut Window, cx: &mut Context<Self>) {
        self.master_volume = volume.clamp(0.0, 1.0);
        if self.master_volume > 0.0 {
            self.muted = false;
        }
        self.audio
            .set_master_volume(if self.muted { 0.0 } else { self.master_volume });
        cx.notify();
    }

    fn toggle_mic_monitor(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled && self.input_device.is_none() {
            return;
        }
        self.mic_monitor = enabled;
        self.audio.set_mic_monitor(enabled);
        self.save_state();
        cx.notify();
    }

    /// Keep the sounds on the headphones and the microphone on the voice.
    fn toggle_headphones_only(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if enabled && self.monitor_device.is_none() {
            window.push_notification(
                "Pick a headphone monitor before routing the bank to the headphones.",
                cx,
            );
            cx.notify();
            return;
        }
        self.headphones_only = enabled;
        self.audio.set_headphones_only(enabled);
        self.save_state();
        cx.notify();
    }

    /// The headphone route dies with the headphones: drop it when the monitor
    /// device goes away so sounds never end up muted by accident.
    fn drop_headphones_only(&mut self) {
        if self.headphones_only {
            self.headphones_only = false;
            self.audio.set_headphones_only(false);
        }
    }

    fn refresh_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.output_devices = audio::list_output_devices();
        self.input_devices = audio::list_input_devices();

        let output_labels = device_labels(&self.output_devices);
        let output_index = self
            .output_device
            .as_ref()
            .and_then(|name| {
                self.output_devices
                    .iter()
                    .position(|choice| choice.name == *name)
            })
            .map(|index| IndexPath::default().row(index));
        self.output_select.update(cx, |state, cx| {
            state.set_items(output_labels, window, cx);
            state.set_selected_index(output_index, window, cx);
        });

        let input_labels = device_labels(&self.input_devices);
        let input_index = self
            .input_device
            .as_ref()
            .and_then(|name| {
                self.input_devices
                    .iter()
                    .position(|choice| choice.name == *name)
            })
            .map(|index| IndexPath::default().row(index));
        self.input_select.update(cx, |state, cx| {
            state.set_items(input_labels, window, cx);
            state.set_selected_index(input_index, window, cx);
        });

        if self.output_device.is_none() {
            let fallback = self
                .output_devices
                .iter()
                .position(|choice| choice.is_default)
                .or(if self.output_devices.is_empty() {
                    None
                } else {
                    Some(0)
                });
            if let Some(index) = fallback {
                self.choose_output(index, window, cx);
            }
        } else if self.output_error_if_missing() {
            self.audio_error = Some("The selected output is no longer available".into());
        }

        if self.input_device.is_none() {
            let fallback = self
                .input_devices
                .iter()
                .position(|choice| choice.is_default)
                .or(if self.input_devices.is_empty() {
                    None
                } else {
                    Some(0)
                });
            if let Some(index) = fallback {
                self.choose_input(index, window, cx);
            }
        }

        if self.input_device.is_none() && self.mic_monitor {
            self.mic_monitor = false;
            self.audio.set_mic_monitor(false);
        }

        self.sync_monitor_devices(window, cx);
        window.push_notification("Audio devices refreshed.", cx);
        cx.notify();
    }

    fn output_error_if_missing(&self) -> bool {
        self.output_device.as_ref().is_some_and(|name| {
            !self
                .output_devices
                .iter()
                .any(|choice| choice.name == *name)
        })
    }

    fn open_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, window| {
            let handles = rfd::AsyncFileDialog::new()
                .set_title("Add sounds to the bank")
                .add_filter("Audio", &["mp3", "wav", "ogg", "flac"])
                .pick_files()
                .await;
            if let Some(handles) = handles {
                let paths: Vec<PathBuf> = handles
                    .into_iter()
                    .map(|handle| handle.path().to_path_buf())
                    .collect();
                let _ = this.update_in(window, |this, window, cx| {
                    this.add_paths(paths, window, cx);
                });
            }
        })
        .detach();
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let mut added = 0usize;
        let mut duplicates = 0usize;
        let mut unsupported = 0usize;
        let mut failed = 0usize;

        for path in paths {
            if !path.is_file() || !is_supported_audio(&path) {
                unsupported += 1;
                continue;
            }
            let path = normalize_path(&path);
            let id = sound_id(&path);
            if self.sounds.iter().any(|sound| sound.id == id) {
                duplicates += 1;
                continue;
            }
            let mut sound = Sound::new(path.clone(), 1.0);
            let metadata_ok = match fs::metadata(&path) {
                Ok(_) => true,
                Err(error) => {
                    sound.set_failed(error.to_string());
                    failed += 1;
                    false
                }
            };
            self.sounds.push(sound);
            self.attach_volume_slider(id, 1.0, window, cx);
            if metadata_ok {
                self.load_sound(id, path.clone(), true, window, cx);
            }
            if self.selected_id.is_none() {
                self.selected_id = Some(id);
            }
            added += 1;
        }

        if added == 0 && duplicates == 0 && unsupported == 0 {
            return;
        }

        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!(
                "{added} {}",
                if added == 1 {
                    "sound added"
                } else {
                    "sounds added"
                }
            ));
        }
        if duplicates > 0 {
            parts.push(format!("{duplicates} already in the bank"));
        }
        if unsupported > 0 {
            parts.push(format!(
                "{unsupported} {}",
                if unsupported == 1 {
                    "format skipped"
                } else {
                    "formats skipped"
                }
            ));
        }
        if failed > 0 {
            parts.push(format!(
                "{failed} {}",
                if failed == 1 {
                    "unreadable file"
                } else {
                    "unreadable files"
                }
            ));
        }
        let message = parts.join(" · ");
        window.push_notification(message, cx);
        self.save_state();
        cx.notify();
    }

    fn remove_sound(&mut self, id: SoundId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.sounds.iter().position(|sound| sound.id == id) else {
            return;
        };
        self.audio.stop_sound(id);
        if self.trim_target == Some(id) {
            self.trim_preview_active = false;
        }
        self.sounds.remove(index);
        self.pending_trims.retain(|pending| *pending != id);
        let rename_open = self.rename_target == Some(id);
        let trim_open = self.trim_target == Some(id);
        if rename_open {
            self.rename_target = None;
        }
        if trim_open {
            self.trim_target = None;
            self.trim_drag = None;
            self.seek_dragging = false;
        }
        if self.selected_id == Some(id) {
            let next = self
                .sounds
                .get(index)
                .or_else(|| self.sounds.get(index.saturating_sub(1)))
                .map(|sound| sound.id);
            self.selected_id = next;
        }
        if self
            .transport
            .as_ref()
            .is_some_and(|current| current.sound_id == id)
        {
            self.transport = None;
            self.paused = false;
            self.playback_elapsed = 0.0;
            self.seek_dragging = false;
            self.seek_slider.update(cx, |slider, cx| {
                slider.set_value(0.0, window, cx);
            });
        }
        if self.sounds.is_empty() {
            self.search_query.clear();
            self.page = 0;
            self.search_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
        }
        self.clamp_page(self.per_page(window));
        self.save_state();
        if trim_open {
            self.maybe_open_next_trim(window, cx);
        }
        window.push_notification("Sound removed from the bank.", cx);
        cx.notify();
    }

    fn move_sound(&mut self, dragged: SoundId, target: SoundId, cx: &mut Context<Self>) {
        if dragged == target {
            return;
        }
        let Some(from) = self.sounds.iter().position(|sound| sound.id == dragged) else {
            return;
        };
        let Some(to) = self.sounds.iter().position(|sound| sound.id == target) else {
            return;
        };
        let sound = self.sounds.remove(from);
        self.sounds.insert(to.min(self.sounds.len()), sound);
        self.save_state();
        cx.notify();
    }

    fn save_state(&self) {
        let Some(path) = state_path() else {
            return;
        };
        let state = PersistedState {
            version: STATE_VERSION,
            sounds: self
                .sounds
                .iter()
                .map(|sound| PersistedSound {
                    path: sound.path.to_string_lossy().to_string(),
                    volume: sound.volume,
                    title: Some(sound.title.to_string()),
                    trim_start: Some(sound.trim_start),
                    trim_end: Some(sound.trim_end),
                    shortcut: sound.shortcut.clone(),
                })
                .collect(),
            selected_path: self
                .selected_sound()
                .map(|sound| sound.path.to_string_lossy().to_string()),
            master_volume: Some(self.master_volume),
            mic_monitor: Some(self.mic_monitor),
            mic_gain: Some(self.mic_gain),
            looping: Some(self.looping),
            output_device: self.output_device.clone(),
            input_device: self.input_device.clone(),
            monitor_device: self.monitor_device.clone(),
            monitor_enabled: Some(self.monitor_device.is_some()),
            per_page: self.page_size.saved(),
            headphones_only: Some(self.headphones_only),
            polyphonic: Some(self.polyphonic),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(serialized) = serde_json::to_string_pretty(&state) {
            match fs::write(&path, serialized) {
                Ok(()) => {}
                Err(error) => eprintln!("Could not save state to {}: {error}", path.display()),
            }
        }
    }

    fn input_summary(&self) -> SharedString {
        self.input_device
            .clone()
            .unwrap_or_else(|| "No microphone".to_string())
            .into()
    }

    fn volume_summary(&self) -> String {
        if self.muted {
            "Muted".into()
        } else {
            format_percent(self.master_volume)
        }
    }

    fn playhead_ratio(&self) -> f32 {
        if self.seek_dragging {
            return self.seek_preview.clamp(0.0, 1.0);
        }
        self.transport
            .as_ref()
            .map(|current| {
                if current.duration > 0.0 {
                    (current.position / current.duration).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0)
    }

    fn playhead_time(&self) -> String {
        if self.seek_dragging {
            let duration = self
                .transport
                .as_ref()
                .map(|current| current.duration)
                .or_else(|| self.selected_sound().map(|sound| sound.duration()))
                .unwrap_or(0.0);
            return crate::model::format_elapsed(duration * self.playhead_ratio());
        }
        self.transport
            .as_ref()
            .map(|current| crate::model::format_elapsed(current.position))
            .unwrap_or_else(|| "00:00".into())
    }

    fn selected_duration(&self) -> f32 {
        self.transport
            .as_ref()
            .map(|current| current.duration)
            .or_else(|| self.selected_sound().map(|sound| sound.duration()))
            .unwrap_or(0.0)
    }

    fn on_play_pause_action(&mut self, _: &PlayPause, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.toggle_playback(window, cx);
    }

    fn on_stop_action(&mut self, _: &StopPlayback, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open {
            self.close_settings(cx);
            return;
        }
        if self.shortcut_target.is_some() {
            self.cancel_shortcut_capture(cx);
            return;
        }
        if self.rename_target.is_some() {
            self.cancel_rename(window, cx);
            return;
        }
        if self.trim_target.is_some() {
            self.cancel_trim(window, cx);
            return;
        }
        self.stop_playback(window, cx);
    }

    fn on_next_action(&mut self, _: &NextSound, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.skip(1, window, cx);
    }

    fn on_previous_action(
        &mut self,
        _: &PreviousSound,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.previous(window, cx);
    }

    fn on_add_action(&mut self, _: &AddSounds, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.open_file_dialog(window, cx);
    }

    fn on_loop_action(&mut self, _: &ToggleLoop, _: &mut Window, cx: &mut Context<Self>) {
        if self.settings_open || self.shortcut_target.is_some() {
            return;
        }
        self.toggle_loop(cx);
    }
}

impl Drop for SoundboardApp {
    fn drop(&mut self) {
        self.save_state();
    }
}

fn playback_watchdog_expired(elapsed: f32, duration: f32) -> bool {
    duration > 0.0 && elapsed + 0.05 >= duration
}

fn build_monitor_devices(
    output_devices: &[DeviceChoice],
    current_output: Option<&str>,
) -> Vec<DeviceChoice> {
    let mut devices: Vec<DeviceChoice> = output_devices
        .iter()
        .filter(|choice| !choice.is_virtual && current_output != Some(choice.name.as_str()))
        .cloned()
        .collect();
    devices.sort_by(|a, b| {
        (a.is_virtual, !a.is_default, &a.name).cmp(&(b.is_virtual, !b.is_default, &b.name))
    });
    devices
}

fn auto_monitor_device(devices: &[DeviceChoice]) -> Option<String> {
    devices
        .iter()
        .find(|choice| {
            let name = choice.name.to_lowercase();
            name.contains("headphone") || name.contains("headset") || name.contains("cuffie")
        })
        .or_else(|| devices.iter().find(|choice| !choice.is_virtual))
        .map(|choice| choice.name.clone())
}

fn monitor_device_labels(devices: &[DeviceChoice]) -> Vec<SharedString> {
    let mut labels = vec![SharedString::from("None")];
    labels.extend(
        devices
            .iter()
            .map(|device| SharedString::from(device.label())),
    );
    labels
}

fn device_labels(devices: &[DeviceChoice]) -> Vec<SharedString> {
    devices
        .iter()
        .map(|device| SharedString::from(device.label()))
        .collect()
}

fn normalize_path(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Roaming"))
        })
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })?;
    Some(base.join(profile_directory()).join("state.json"))
}

fn profile_directory() -> &'static str {
    if cfg!(debug_assertions) {
        "SimpleSoundboard-Dev"
    } else {
        "SimpleSoundboard"
    }
}

fn load_state() -> PersistedState {
    let Some(path) = state_path() else {
        return PersistedState::default();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return PersistedState::default();
    };
    serde_json::from_str::<PersistedState>(&text)
        .ok()
        .filter(|state| state.version == 0 || state.version <= STATE_VERSION)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        LibraryMetrics, PageSize, auto_monitor_device, build_monitor_devices,
        playback_watchdog_expired, state_path, waveform_bar_filled, waveform_ratio,
        waveform_seek_ratio,
    };
    use crate::audio::DeviceChoice;
    use gpui_kit::{Bounds, point, px, size};
    use std::path::Path;

    fn auto_page_size_for(width: f32, height: f32, rem: f32) -> usize {
        LibraryMetrics::for_viewport(width, height, rem).per_page
    }

    #[test]
    fn waveform_clicks_map_to_positions_inside_the_clip() {
        let bounds = Bounds {
            origin: point(px(35.0), px(208.0)),
            size: size(px(200.0), px(22.0)),
        };
        assert_eq!(
            waveform_ratio(&bounds, point(px(35.0), px(219.0))),
            Some(0.0)
        );
        assert_eq!(
            waveform_ratio(&bounds, point(px(85.0), px(219.0))),
            Some(0.25)
        );
        assert_eq!(
            waveform_ratio(&bounds, point(px(135.0), px(219.0))),
            Some(0.5)
        );
        assert_eq!(
            waveform_ratio(&bounds, point(px(185.0), px(219.0))),
            Some(0.75)
        );
        // Clicks outside the row still toggle the pad.
        assert_eq!(waveform_ratio(&bounds, point(px(235.0), px(219.0))), None);
        assert_eq!(waveform_ratio(&bounds, point(px(35.0), px(180.0))), None);
        assert_eq!(waveform_ratio(&bounds, point(px(300.0), px(219.0))), None);
    }

    #[test]
    fn waveform_seek_zone_includes_a_small_margin_below_the_bars() {
        let bounds = Bounds {
            origin: point(px(35.0), px(208.0)),
            size: size(px(200.0), px(22.0)),
        };
        assert_eq!(
            waveform_seek_ratio(&bounds, point(px(135.0), px(219.0))),
            Some(0.5)
        );
        // The pointer may sit a few pixels under the bars while the row grows
        // toward it without losing the preview.
        assert_eq!(
            waveform_seek_ratio(&bounds, point(px(85.0), px(228.0))),
            Some(0.25)
        );
        // Above the row, or far below it, is the rest of the pad.
        assert_eq!(
            waveform_seek_ratio(&bounds, point(px(85.0), px(205.0))),
            None
        );
        assert_eq!(
            waveform_seek_ratio(&bounds, point(px(85.0), px(240.0))),
            None
        );
        assert_eq!(
            waveform_seek_ratio(&bounds, point(px(300.0), px(219.0))),
            None
        );
    }

    #[test]
    fn waveform_fill_uses_bar_centres() {
        // Four bars: a half-way position lights the first two.
        assert!(waveform_bar_filled(0, 4, 0.5));
        assert!(waveform_bar_filled(1, 4, 0.5));
        assert!(!waveform_bar_filled(2, 4, 0.5));
        assert!(!waveform_bar_filled(3, 4, 0.5));
        // The very start and the very end fill nothing and everything.
        assert!(!waveform_bar_filled(0, 4, 0.0));
        assert!(waveform_bar_filled(3, 4, 1.0));
        assert!(!waveform_bar_filled(0, 0, 1.0));
    }

    #[test]
    fn release_notes_split_headings_bullets_and_text() {
        use super::view::{ReleaseNote, release_note_lines};

        let notes = release_note_lines(
            "## Fixes\n- **Audio**: the pop is gone\n- Headphones only added\nPlain line\n\n---\n",
        );
        assert_eq!(
            notes,
            vec![
                ReleaseNote::Heading("Fixes".into()),
                ReleaseNote::Bullet("Audio: the pop is gone".into()),
                ReleaseNote::Bullet("Headphones only added".into()),
                ReleaseNote::Text("Plain line".into()),
            ]
        );
        assert!(release_note_lines("").is_empty());
        assert!(release_note_lines("---\n\n***").is_empty());
    }

    #[test]
    fn auto_page_size_fills_the_library_with_whole_rows() {
        // Default 1280×860 window: three rows of three pads fit.
        assert_eq!(auto_page_size_for(1280.0, 860.0, 15.0), 9);
        // Minimum 1120×760 window: only two rows fit.
        assert_eq!(auto_page_size_for(1120.0, 760.0, 15.0), 6);
        // A taller window gains a row.
        assert_eq!(auto_page_size_for(1280.0, 1080.0, 15.0), 12);
        // A wider window gains columns instead of stretching the pads.
        assert_eq!(auto_page_size_for(1920.0, 860.0, 15.0), 18);
        assert_eq!(auto_page_size_for(2560.0, 1392.0, 15.0), 40);
        // Smaller windows never go below a single pad.
        assert_eq!(auto_page_size_for(400.0, 300.0, 15.0), 1);
    }

    #[test]
    fn library_pads_never_overflow_their_row() {
        for (width, height) in [
            (1120.0, 760.0),
            (1280.0, 860.0),
            (1600.0, 1000.0),
            (1920.0, 1080.0),
            (2560.0, 1392.0),
            (3840.0, 2160.0),
        ] {
            let metrics = LibraryMetrics::for_viewport(width, height, 15.0);
            let rem = 15.0;
            let content_width = width - 20.0 * rem - 2.5 * rem;
            let row_width = metrics.columns as f32 * metrics.tile_width
                + (metrics.columns - 1) as f32 * 0.75 * rem;
            assert!(
                row_width <= content_width,
                "{width}×{height}: {row_width} > {content_width}"
            );
            assert!(metrics.tile_width >= 15.5 * rem);
        }
    }

    #[test]
    fn page_size_round_trips_through_saved_state() {
        assert_eq!(PageSize::from_saved(None), PageSize::Auto);
        assert_eq!(PageSize::from_saved(Some(6)), PageSize::Fixed(6));
        assert_eq!(PageSize::from_saved(Some(7)), PageSize::Auto);
        assert_eq!(PageSize::Auto.saved(), None);
        assert_eq!(PageSize::Fixed(9).saved(), Some(9));
        assert_eq!(PageSize::from_label("Auto"), Some(PageSize::Auto));
        assert_eq!(PageSize::from_label("12"), Some(PageSize::Fixed(12)));
        assert_eq!(PageSize::from_label("7"), None);
        assert_eq!(PageSize::Fixed(24).index(), 6);
    }

    #[test]
    fn state_path_is_outside_the_project_when_appdata_exists() {
        if std::env::var_os("APPDATA").is_some() {
            let path = state_path().expect("state path");
            let expected = if cfg!(debug_assertions) {
                "SimpleSoundboard-Dev/state.json"
            } else {
                "SimpleSoundboard/state.json"
            };
            assert!(path.ends_with(expected));
        }
    }

    #[test]
    fn playback_watchdog_expires_only_at_the_end() {
        assert!(!playback_watchdog_expired(1.94, 2.0));
        assert!(playback_watchdog_expired(1.96, 2.0));
        assert!(playback_watchdog_expired(2.2, 2.0));
        assert!(!playback_watchdog_expired(0.0, 0.0));
    }

    #[test]
    fn monitor_preferences_exclude_the_main_and_virtual_devices() {
        let devices = vec![
            DeviceChoice {
                name: "SteelSeries Sonar".into(),
                is_default: true,
                is_virtual: true,
            },
            DeviceChoice {
                name: "Cuffie USB".into(),
                is_default: false,
                is_virtual: false,
            },
            DeviceChoice {
                name: "Speakers".into(),
                is_default: true,
                is_virtual: false,
            },
        ];
        let monitors = build_monitor_devices(&devices, Some("Cuffie USB"));
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].name, "Speakers");
        assert_eq!(auto_monitor_device(&devices), Some("Cuffie USB".into()));
    }

    #[test]
    fn wav_paths_are_supported() {
        assert!(crate::model::is_supported_audio(Path::new("clip.mp3")));
        assert!(crate::model::is_supported_audio(Path::new("clip.wav")));
        assert!(!crate::model::is_supported_audio(Path::new("notes.txt")));
    }
}
