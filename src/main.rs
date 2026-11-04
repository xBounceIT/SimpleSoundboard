#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod model;
mod theme;
mod update;

use app::{AddSounds, FocusSearch, PlayPause, SoundboardApp, StopPlayback, ToggleLoop};
use gpui_kit::*;

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            theme::install(cx);

            cx.bind_keys([
                KeyBinding::new("ctrl-p", PlayPause, None),
                KeyBinding::new("escape", StopPlayback, None),
                KeyBinding::new("ctrl-l", ToggleLoop, None),
                KeyBinding::new("ctrl-o", AddSounds, None),
                KeyBinding::new("ctrl-f", FocusSearch, None),
            ]);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1280.), px(860.)), cx)),
                window_min_size: Some(size(px(1120.), px(760.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("SimpleSoundboard".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                app_id: Some("SimpleSoundboard".into()),
                ..WindowOptions::default()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| SoundboardApp::new(window, cx))
            })
            .expect("failed to open the soundboard window");

            cx.activate(true);
        });
}
