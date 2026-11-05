use std::{cell::Cell, rc::Rc};

use gpui_kit::assets::IconName as AppIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    button::ButtonCustomVariant,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{ContextMenuExt as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    select::Select,
    slider::Slider,
    switch::Switch,
    v_flex,
};
use gpui_kit::{
    AnimationExt as _, AnyElement, App, AppContext as _, Bounds, ClickEvent, CursorStyle,
    ExternalPaths, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PathBuilder,
    Render, SharedString, SpringAnimation, SpringConfig, StatefulInteractiveElement as _,
    Styled as _, Window, WindowControlArea, canvas, div, point, prelude::FluentBuilder as _, px,
    quad, relative, rems, size, transparent_black,
};

use super::{SoundboardApp, TrimDrag};
use crate::{
    model::{
        Sound, SoundId, SoundStatus, format_duration, format_percent, format_time_precise,
        normalize_peak, peak_to_db,
    },
    theme::{BODY_FONT, DISPLAY_FONT, MONO_FONT, Palette, palette},
    update,
};

impl Render for SoundboardApp {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        div()
            .id("soundboard")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.paper)
            .text_color(p.ink)
            .font_family(BODY_FONT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_play_pause_action))
            .on_action(cx.listener(Self::on_stop_action))
            .on_action(cx.listener(Self::on_next_action))
            .on_action(cx.listener(Self::on_previous_action))
            .on_action(cx.listener(Self::on_add_action))
            .on_action(cx.listener(Self::on_loop_action))
            .on_action(cx.listener(Self::on_focus_search))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_shortcut_key(event, window, cx);
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_paths(paths.0.to_vec(), window, cx);
            }))
            .child(self.render_title_bar(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_stretch()
                    .child(self.render_library(window, cx))
                    .child(self.render_routing(cx)),
            )
            .child(self.render_transport(cx))
            .child(self.render_update_toast(cx))
            .child(self.render_shortcut_capture(cx))
    }
}

impl SoundboardApp {
    fn render_shortcut_capture(&self, cx: &mut gpui_kit::Context<Self>) -> AnyElement {
        let p = palette();
        let Some(id) = self.shortcut_target else {
            return div().into_any_element();
        };
        let Some(sound) = self.sounds.iter().find(|sound| sound.id == id) else {
            return div().into_any_element();
        };

        div()
            .absolute()
            .top(px(76.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                h_flex()
                    .w(px(520.0))
                    .items_start()
                    .gap_3()
                    .p_3()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(p.amber.opacity(0.7))
                    .bg(p.paper_mid)
                    .shadow_lg()
                    .child(Icon::new(AppIcon::Keyboard).size_5().text_color(p.amber))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.amber)
                                    .child("KEYBOARD SHORTCUT"),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(DISPLAY_FONT)
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.cream)
                                    .child(format!("Press the shortcut for “{}”", sound.title)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .line_height(relative(1.4))
                                    .text_color(p.ink_muted)
                                    .child("Ctrl or Alt + key, or F1–F12 · Esc to cancel"),
                            )
                            .when_some(self.shortcut_error.as_ref(), |this, error| {
                                this.child(
                                    div()
                                        .rounded(cx.theme().radius)
                                        .bg(p.signal_soft)
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(p.cream)
                                        .child(error.clone()),
                                )
                            }),
                    )
                    .child(
                        Button::new("cancel-shortcut")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_shortcut_capture(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_update_toast(&self, cx: &mut gpui_kit::Context<Self>) -> AnyElement {
        let p = palette();
        let Some(available) = self.available_update.as_ref() else {
            return div().into_any_element();
        };
        if self.update_dismissed {
            return div().into_any_element();
        }

        let downloading = self.update_downloading;
        let close = Button::new("dismiss-update")
            .icon(Icon::new(AppIcon::X))
            .small()
            .ghost()
            .accessibility_label("Dismiss update notification")
            .disabled(downloading)
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_update(cx)));

        let update_button = Button::new("update-now")
            .label(if downloading {
                "Downloading…"
            } else {
                "Update now"
            })
            .primary()
            .small()
            .disabled(downloading)
            .on_click(cx.listener(|this, _, window, cx| this.install_update(window, cx)));

        let notes_button = Button::new("update-notes")
            .label("See changes")
            .ghost()
            .small()
            .on_click(cx.listener(|this, _, _, cx| this.open_update_notes(cx)));

        div()
            .absolute()
            .right(px(24.0))
            .bottom(px(112.0))
            .w(px(404.0))
            .v_flex()
            .gap_3()
            .p_4()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(p.blue.opacity(0.55))
            .bg(p.paper_mid)
            .shadow_lg()
            .child(
                h_flex()
                    .justify_between()
                    .items_start()
                    .gap_3()
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.blue)
                                    .child("UPDATE AVAILABLE"),
                            )
                            .child(
                                div()
                                    .font_family(DISPLAY_FONT)
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .line_height(relative(1.15))
                                    .text_color(p.cream)
                                    .child(format!(
                                        "SimpleSoundboard v{} is now available",
                                        available.version
                                    )),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(relative(1.4))
                                    .text_color(p.ink_muted)
                                    .child(if downloading {
                                        "Downloading the setup… The app will close and reopen automatically.".to_string()
                                    } else {
                                        format!(
                                            "You are on v{}. Download and install the update automatically.",
                                            update::CURRENT_VERSION
                                        )
                                    }),
                            ),
                    )
                    .child(close),
            )
            .when_some(self.update_error.as_ref(), |this, error| {
                this.child(
                    div()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(p.signal.opacity(0.4))
                        .bg(p.signal_soft)
                        .px_3()
                        .py_2()
                        .text_xs()
                        .line_height(relative(1.4))
                        .text_color(p.cream)
                        .child(error.clone()),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .items_center()
                    .gap_2()
                    .child(notes_button)
                    .child(update_button),
            )
            .into_any_element()
    }
}

impl SoundboardApp {
    fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let expanded = window.is_maximized() || window.is_fullscreen();

        let drag_zone = h_flex()
            .id("title-drag-zone")
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_4()
            .pl_5()
            .window_control_area(WindowControlArea::Drag)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .size_10()
                            .rounded(cx.theme().radius_lg)
                            .bg(p.signal)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(AppIcon::AudioLines).size_5().text_color(p.cream)),
                    )
                    .child(
                        div()
                            .font_family(DISPLAY_FONT)
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.cream)
                            .child("SimpleSoundboard"),
                    )
                    .when(cfg!(debug_assertions), |this| {
                        this.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(cx.theme().radius)
                                .border_1()
                                .border_color(p.amber.opacity(0.45))
                                .font_family(MONO_FONT)
                                .text_xs()
                                .text_color(p.amber)
                                .child("DEV"),
                        )
                    }),
            );

        let minimize = window_control_button(
            cx,
            "window-minimize",
            AppIcon::Minus,
            p.console_hi,
            p.console_line,
            "Minimize",
            WindowControlAction::Minimize,
        );

        let maximize = window_control_button(
            cx,
            "window-maximize",
            if expanded {
                AppIcon::Minimize
            } else {
                AppIcon::Maximize
            },
            p.console_hi,
            p.console_line,
            if expanded { "Restore" } else { "Maximize" },
            WindowControlAction::Maximize,
        );

        let close = window_control_button(
            cx,
            "window-close",
            AppIcon::X,
            p.signal,
            p.signal,
            "Close",
            WindowControlAction::Close,
        );

        let controls = h_flex()
            .h_full()
            .flex_shrink_0()
            .child(minimize)
            .child(maximize)
            .child(close);

        h_flex()
            .h_16()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .bg(p.console)
            .border_b_1()
            .border_color(p.console_line)
            .child(drag_zone)
            .child(controls)
    }

    fn render_library(
        &mut self,
        window: &Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let trim_open = self.trim_target.is_some();

        let body = if self.sounds.is_empty() {
            self.render_empty_state(cx).into_any_element()
        } else {
            let per_page = self.per_page(window).max(1);
            let filtered = self.filtered_sound_indices();
            let total_pages = filtered.len().div_ceil(per_page).max(1);
            let page = self.page.min(total_pages - 1);
            self.page = page;
            let start = page * per_page;
            let end = (start + per_page).min(filtered.len());
            let page_indices = filtered[start..end].to_vec();
            let list = if filtered.is_empty() {
                self.render_no_results(cx).into_any_element()
            } else {
                self.render_grid(&page_indices, cx).into_any_element()
            };
            let count = filtered.len();

            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_library_toolbar(count, cx))
                .child(list)
                .when(count > 0, |this| {
                    this.child(self.render_pagination(count, per_page, cx))
                })
                .into_any_element()
        };
        let editor = if trim_open {
            Some(self.render_inline_editor(cx))
        } else {
            None
        };

        let mut root = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(p.paper);
        if let Some(editor) = editor {
            root = root.child(editor);
        }
        root.child(body)
    }

    fn render_library_toolbar(
        &mut self,
        count: usize,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let count_label = match count {
            0 => "No sounds".to_string(),
            1 => "1 sound".to_string(),
            count => format!("{count} sounds"),
        };

        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .px_5()
            .pt_5()
            .pb_2()
            .child(
                div().w(px(320.0)).child(
                    Input::new(&self.search_input)
                        .prefix(Icon::new(AppIcon::Search).size_4().text_color(p.ink_muted))
                        .cleanable(true),
                ),
            )
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_xs()
                    .text_color(p.ink_muted)
                    .child(count_label),
            )
            .child(div().flex_1())
            .child(
                Button::new("add-sounds-toolbar")
                    .label("Add sounds")
                    .icon(Icon::new(AppIcon::Plus))
                    .primary()
                    .small()
                    .tooltip("Add sounds · Ctrl+O")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_file_dialog(window, cx);
                    })),
            )
    }

    fn render_no_results(&mut self, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        let query = self.search_query.trim().to_string();

        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                v_flex()
                    .max_w_96()
                    .items_center()
                    .gap_3()
                    .child(Icon::new(AppIcon::Search).size_8().text_color(p.ink_muted))
                    .child(
                        div()
                            .font_family(DISPLAY_FONT)
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.cream)
                            .child("No sounds found"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .line_height(relative(1.5))
                            .text_color(p.ink_muted)
                            .child(format!("No titles match “{query}”.")),
                    )
                    .child(
                        Button::new("clear-search")
                            .label("Clear search")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_search(window, cx);
                            })),
                    ),
            )
    }

    fn render_pagination(
        &mut self,
        count: usize,
        per_page: usize,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let total_pages = count.div_ceil(per_page.max(1)).max(1);
        let page = self.page.min(total_pages - 1);
        let first = page * per_page + 1;
        let last = ((page + 1) * per_page).min(count);

        let previous = Button::new("page-previous")
            .icon(Icon::new(AppIcon::ChevronLeft))
            .small()
            .ghost()
            .accessibility_label("Previous page")
            .tooltip("Previous page")
            .disabled(page == 0)
            .on_click(cx.listener(|this, _, _, cx| this.previous_page(cx)));

        let next = Button::new("page-next")
            .icon(Icon::new(AppIcon::ChevronRight))
            .small()
            .ghost()
            .accessibility_label("Next page")
            .tooltip("Next page")
            .disabled(page + 1 >= total_pages)
            .on_click(cx.listener(move |this, _, _, cx| this.next_page(per_page, cx)));

        let page_size_select = Select::new(&self.page_size_select)
            .small()
            .accessibility_label("Sounds per page");

        // `auto_page_size` in app.rs assumes this footer is exactly 3rem tall:
        // `py_3` around small controls. Keep the two in sync.
        h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .gap_3()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(p.line)
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_xs()
                    .text_color(p.ink_muted)
                    .child(format!("SHOWING {first}–{last} OF {count}")),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(previous)
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(p.ink_muted)
                            .child(format!("PAGE {} OF {}", page + 1, total_pages)),
                    )
                    .child(next),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("PER PAGE", p.ink_muted))
                    .child(div().w(px(104.0)).child(page_size_select)),
            )
    }

    fn render_inline_editor(&mut self, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some(id) = self.trim_target else {
            return div().into_any_element();
        };
        let Some(sound) = self.sounds.iter().find(|sound| sound.id == id) else {
            return div().into_any_element();
        };
        let duration = sound.full_duration();
        let start_seconds = duration * self.trim_start_pct / 100.0;
        let end_seconds = duration * self.trim_end_pct / 100.0;
        let selected_duration = (end_seconds - start_seconds).max(0.0);
        let waveform = sound
            .source
            .as_ref()
            .map(|source| source.samples.as_ref())
            .unwrap_or(&[]);
        let peaks = peak_bars(waveform, 260);
        let title = sound.title.clone();
        let start_fraction = self.trim_start_pct / 100.0;
        let end_fraction = self.trim_end_pct / 100.0;
        let dragging = self.trim_drag.is_some();
        let preview_active = self.trim_preview_active;
        let preview_ratio = if preview_active {
            self.transport
                .as_ref()
                .filter(|current| current.sound_id == id)
                .map(|current| {
                    if current.duration > 0.0 {
                        (current.position / current.duration).clamp(0.0, 1.0)
                    } else {
                        0.0
                    }
                })
        } else {
            None
        };
        let app = cx.entity().downgrade();
        let bounds_store: Rc<Cell<Option<Bounds<gpui_kit::Pixels>>>> = Rc::new(Cell::new(None));

        let track = div()
            .id("trim-overlay")
            .relative()
            .h_16()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(p.line_strong)
            .bg(p.console)
            .overflow_hidden()
            .cursor_ew_resize()
            .on_mouse_down(MouseButton::Left, {
                let bounds_store = bounds_store.clone();
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let Some(bounds) = bounds_store.get() else {
                        return;
                    };
                    let width = f32::from(bounds.size.width).max(1.0);
                    let inset = 6.0f32;
                    let usable = (width - inset * 2.0).max(1.0);
                    let x = (f32::from(event.position.x) - f32::from(bounds.origin.x) - inset)
                        .clamp(0.0, usable);
                    let pointer_pct = x / usable * 100.0;
                    let start_x = usable * this.trim_start_pct / 100.0;
                    let end_x = usable * this.trim_end_pct / 100.0;
                    let tolerance = 14.0;
                    let kind = if (x - start_x).abs() <= tolerance {
                        TrimDrag::Start
                    } else if (x - end_x).abs() <= tolerance {
                        TrimDrag::End
                    } else if pointer_pct > this.trim_start_pct && pointer_pct < this.trim_end_pct {
                        TrimDrag::Window {
                            grab: pointer_pct - this.trim_start_pct,
                        }
                    } else if (x - start_x).abs() <= (x - end_x).abs() {
                        TrimDrag::Start
                    } else {
                        TrimDrag::End
                    };
                    this.begin_trim_drag(kind, pointer_pct, window, cx);
                })
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_trim_drag(cx);
                }),
            )
            .child(
                canvas(|bounds, _, _| bounds, {
                    let bounds_store = bounds_store.clone();
                    let app = app.clone();
                    let peaks = peaks.clone();
                    move |bounds, _, window, _cx| {
                        bounds_store.set(Some(bounds));
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let origin_x = f32::from(bounds.origin.x);
                        let origin_y = f32::from(bounds.origin.y);
                        if width <= 0.0 || height <= 0.0 || peaks.is_empty() {
                            return;
                        }
                        let inset = 6.0f32;
                        let usable = (width - inset * 2.0).max(1.0);
                        let center = origin_y + height * 0.5;
                        let start_x = origin_x + inset + usable * start_fraction;
                        let end_x = origin_x + inset + usable * end_fraction;

                        window.paint_quad(quad(
                            Bounds {
                                origin: point(px(start_x), px(origin_y)),
                                size: size(px((end_x - start_x).max(1.0)), px(height)),
                            },
                            px(0.0),
                            p.blue.opacity(0.10),
                            px(0.0),
                            transparent_black(),
                            Default::default(),
                        ));

                        let mut outside = PathBuilder::stroke(px(1.7));
                        let mut inside = PathBuilder::stroke(px(1.7));
                        for (index, peak) in peaks.iter().enumerate() {
                            let position = (index as f32 + 0.5) / peaks.len().max(1) as f32;
                            let x = origin_x + inset + usable * position;
                            let amplitude = (peak.clamp(0.0, 1.0).sqrt() * height * 0.38).max(1.0);
                            let path = if position >= start_fraction && position <= end_fraction {
                                &mut inside
                            } else {
                                &mut outside
                            };
                            path.move_to(point(px(x), px(center - amplitude)));
                            path.line_to(point(px(x), px(center + amplitude)));
                        }
                        if let Ok(path) = outside.build() {
                            window.paint_path(path, p.ink_muted.opacity(0.45));
                        }
                        if let Ok(path) = inside.build() {
                            window.paint_path(path, p.blue);
                        }

                        if let Some(ratio) = preview_ratio {
                            let x = origin_x
                                + inset
                                + usable
                                    * (start_fraction + (end_fraction - start_fraction) * ratio);
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(px(x - 1.0), px(origin_y + 3.0)),
                                    size: size(px(2.0), px((height - 6.0).max(1.0))),
                                },
                                px(0.0),
                                p.signal,
                                px(0.0),
                                transparent_black(),
                                Default::default(),
                            ));
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(px(x - 4.0), px(origin_y + 3.0)),
                                    size: size(px(8.0), px(8.0)),
                                },
                                px(4.0),
                                p.signal,
                                px(0.0),
                                transparent_black(),
                                Default::default(),
                            ));
                        }

                        for boundary in [start_fraction, end_fraction] {
                            let x = origin_x + inset + usable * boundary;
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(px(x - 0.75), px(origin_y)),
                                    size: size(px(1.5), px(height)),
                                },
                                px(0.0),
                                p.blue.opacity(0.5),
                                px(0.0),
                                transparent_black(),
                                Default::default(),
                            ));
                            let cap = Bounds {
                                origin: point(px(x - 5.0), px(center - 12.0)),
                                size: size(px(10.0), px(24.0)),
                            };
                            window.paint_quad(quad(
                                cap,
                                px(4.0),
                                p.cream,
                                px(1.5),
                                p.blue,
                                Default::default(),
                            ));
                            for offset in [-4.0f32, 0.0, 4.0] {
                                window.paint_quad(quad(
                                    Bounds {
                                        origin: point(px(x - 0.75), px(center + offset - 1.5)),
                                        size: size(px(1.5), px(3.0)),
                                    },
                                    px(0.75),
                                    p.blue,
                                    px(0.0),
                                    transparent_black(),
                                    Default::default(),
                                ));
                            }
                        }

                        if dragging {
                            let app_move = app.clone();
                            let app_up = app.clone();
                            let track_bounds = bounds;
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                                if phase.bubble() && event.dragging() {
                                    let width = f32::from(track_bounds.size.width).max(1.0);
                                    let inset = 6.0f32;
                                    let usable = (width - inset * 2.0).max(1.0);
                                    let x = (f32::from(event.position.x)
                                        - f32::from(track_bounds.origin.x)
                                        - inset)
                                        .clamp(0.0, usable);
                                    let pointer_pct = x / usable * 100.0;
                                    let _ = app_move.update(cx, |this, cx| {
                                        this.update_trim_drag(pointer_pct, cx);
                                    });
                                }
                            });
                            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                if phase.bubble() && event.button == MouseButton::Left {
                                    let _ = app_up.update(cx, |this, cx| {
                                        this.end_trim_drag(cx);
                                    });
                                }
                            });
                        }
                    }
                })
                .absolute()
                .inset_0(),
            );

        h_flex()
            .mx_6()
            .mt_3()
            .p_3()
            .gap_3()
            .items_center()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(p.blue.opacity(0.45))
            .bg(p.paper_hi)
            .child(Icon::new(AppIcon::Scissors).size_4().text_color(p.blue))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .v_flex()
                                    .gap_0()
                                    .child(
                                        div()
                                            .font_family(MONO_FONT)
                                            .text_xs()
                                            .text_color(p.ink_muted)
                                            .child("TRIM"),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .font_family(DISPLAY_FONT)
                                            .text_lg()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.ink)
                                            .child(format!("Trim “{title}”")),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_center()
                                    .child(
                                        Button::new("audition-trim")
                                            .label(if preview_active { "Stop" } else { "Audition" })
                                            .icon(Icon::new(if preview_active {
                                                AppIcon::Square
                                            } else {
                                                AppIcon::Play
                                            }))
                                            .small()
                                            .ghost()
                                            .tooltip(if preview_active {
                                                "Stop the trim preview"
                                            } else {
                                                "Audition the selected portion"
                                            })
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.toggle_trim_preview(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .font_family(MONO_FONT)
                                            .text_xs()
                                            .text_color(p.ink_muted)
                                            .child(format!(
                                                "{} → {} · {}",
                                                format_time_precise(start_seconds),
                                                format_time_precise(end_seconds),
                                                format_time_precise(selected_duration)
                                            )),
                                    ),
                            ),
                    )
                    .child(track)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.ink_muted)
                                    .child("DRAG THE HANDLES · DRAG THE CENTER TO MOVE"),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.blue)
                                    .child(format!(
                                        "KEPT {}",
                                        format_time_precise(selected_duration)
                                    )),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("cancel-trim")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_trim(window, cx);
                            })),
                    )
                    .child(
                        Button::new("apply-trim")
                            .label("Apply")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_trim(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_empty_state(&mut self, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        let add = Button::new("add-first-sound")
            .label("Add your first sound…")
            .icon(Icon::new(AppIcon::Plus))
            .primary()
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_file_dialog(window, cx);
            }));

        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                h_flex()
                    .w(relative(0.8))
                    .min_w_0()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(p.line)
                    .bg(p.paper_hi)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_6()
                            .v_flex()
                            .items_start()
                            .gap_4()
                            .child(section_label("EMPTY BANK", p.signal))
                            .child(
                                div()
                                    .font_family(DISPLAY_FONT)
                                    .text_3xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .line_height(relative(1.05))
                                    .child("Put your first\nsound on air."),
                            )
                            .child(
                                div()
                                    .max_w_96()
                                    .text_sm()
                                    .line_height(relative(1.55))
                                    .text_color(p.ink_muted)
                                    .child(
                                        "Add an audio file and turn it into a pad on your console. You can also drop it here.",
                                    ),
                            )
                            .child(add.primary())
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .text_color(p.ink_muted)
                                    .child(Icon::new(AppIcon::FileMusic).size_3p5())
                                    .child(
                                        div()
                                            .font_family(MONO_FONT)
                                            .text_xs()
                                            .child("MP3 · WAV · OGG · FLAC"),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_48()
                            .self_stretch()
                            .p_5()
                            .bg(p.console)
                            .rounded_tr(cx.theme().radius_lg)
                            .rounded_br(cx.theme().radius_lg)
                            .flex()
                            .flex_col()
                            .justify_between()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .font_family(MONO_FONT)
                                            .text_xs()
                                            .text_color(p.cream_muted)
                                            .child("SLOT 01"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded(cx.theme().radius)
                                            .border_1()
                                            .border_color(p.console_line)
                                            .font_family(MONO_FONT)
                                            .text_xs()
                                            .text_color(p.cream_muted)
                                            .child("EMPTY"),
                                    ),
                            )
                            .child(
                                div()
                                    .font_family(DISPLAY_FONT)
                                    .text_2xl()
                                    .text_color(p.cream)
                                    .line_height(relative(1.1))
                                    .child("Waiting for\nthe first file."),
                            )
                            .child(
                                h_flex()
                                    .h_8()
                                    .items_end()
                                    .gap_1()
                                    .children((0..24).map(|index| {
                                        let height = 4.0
                                            + ((index as f32 * 0.73).sin().abs() * 22.0)
                                            + ((index as f32 * 0.21).cos().abs() * 8.0);
                                        div()
                                            .flex_1()
                                            .h(px(height))
                                            .rounded_full()
                                            .bg(if index % 7 == 0 {
                                                p.cream_muted.opacity(0.4)
                                            } else {
                                                p.console_line
                                            })
                                    })),
                            ),
                    ),
            )
    }

    fn render_grid(
        &mut self,
        indices: &[usize],
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let sounds: Vec<(usize, Sound)> = indices
            .iter()
            .filter_map(|&index| self.sounds.get(index).map(|sound| (index, sound.clone())))
            .collect();
        let mut tiles = Vec::with_capacity(sounds.len());
        for (index, sound) in sounds.iter() {
            tiles.push(self.render_sound_tile(*index, sound, cx).into_any_element());
        }

        div()
            .id(("sound-grid", self.page))
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scrollbar()
            .bg(p.paper)
            .child(
                div()
                    .w_full()
                    .px_5()
                    .pt_3()
                    .pb_5()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .children(tiles),
            )
    }

    fn render_sound_tile(
        &mut self,
        index: usize,
        sound: &Sound,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl IntoElement {
        let p = palette();
        let id = sound.id;
        let app = cx.entity().downgrade();
        let selected = self.selected_id == Some(id);
        let hovered = self.hovered_sound == Some(id);
        let tile_bg = if selected {
            p.tile_active
        } else if hovered {
            p.tile_hover
        } else {
            p.tile
        };
        let playing = self.is_sound_playing(id);
        let progress = self
            .transport
            .as_ref()
            .filter(|transport| transport.sound_id == id)
            .map(|transport| {
                if transport.duration > 0.0 {
                    (transport.position / transport.duration).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let loading = matches!(sound.status, SoundStatus::Loading);
        let failed = matches!(sound.status, SoundStatus::Failed(_));
        let title = sound.title.clone();
        let shortcut = sound.shortcut.clone();
        let capturing_shortcut = self.shortcut_target == Some(id);
        let duration = if sound.is_ready() {
            format_duration(sound.duration())
        } else if loading {
            "···".to_string()
        } else {
            "--:--".to_string()
        };
        let format = sound.format_label();
        let waveform = if sound.waveform().is_empty() {
            placeholder_waveform()
        } else {
            sound.waveform().to_vec()
        };

        let tile_icon = if playing {
            AppIcon::Square
        } else {
            AppIcon::Play
        };

        let renaming = self.rename_target == Some(id);
        let volume_slider = self.volume_sliders.get(&id).cloned();
        // The pad overlay owns every click above the volume row. It needs the
        // waveform bounds so a click there can seek instead of toggling.
        let waveform_seek = sound.is_ready() && !renaming;
        let waveform_bounds: Rc<Cell<Option<Bounds<gpui_kit::Pixels>>>> = Rc::new(Cell::new(None));
        // The point under the pointer previews the seek: the row grows and the
        // bars up to the pointer light up.
        let waveform_hover = if waveform_seek {
            self.waveform_hover
                .filter(|hover| hover.sound_id == id)
                .map(|hover| hover.ratio)
        } else {
            None
        };
        let duration_seconds = sound.duration();
        let hover_time = waveform_hover.map(|ratio| format_time_precise(duration_seconds * ratio));
        let content = div()
            .size_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .pr(px(36.0))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(if selected { p.blue } else { p.console_line })
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(if selected { p.cream } else { p.cream_muted })
                                    .child(format!("{:02}", index + 1)),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.cream_muted)
                                    .child(format),
                            ),
                    )
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(if hover_time.is_some() {
                                p.cream
                            } else {
                                p.cream_muted
                            })
                            .child(hover_time.clone().unwrap_or_else(|| duration.clone())),
                    ),
            )
            .child(if renaming {
                div()
                    .w_full()
                    .min_w_0()
                    .child(Input::new(&self.rename_input).w_full())
                    .into_any_element()
            } else {
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(DISPLAY_FONT)
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if failed { p.signal } else { p.cream })
                            .truncate()
                            .child(title.clone()),
                    )
                    .when_some(shortcut.clone(), |this, shortcut| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(108.0))
                                .truncate()
                                .px_1()
                                .py_0p5()
                                .rounded(cx.theme().radius)
                                .border_1()
                                .border_color(if capturing_shortcut {
                                    p.amber
                                } else {
                                    p.console_line
                                })
                                .font_family(MONO_FONT)
                                .text_xs()
                                .text_color(if capturing_shortcut {
                                    p.amber
                                } else {
                                    p.cream_muted
                                })
                                .child(shortcut.label()),
                        )
                    })
                    .into_any_element()
            })
            .child({
                if failed {
                    let error = match &sound.status {
                        SoundStatus::Failed(error) => error.clone(),
                        _ => SharedString::from("Read error"),
                    };
                    div()
                        .relative()
                        .flex_shrink_0()
                        .h_6()
                        .w_full()
                        .child(
                            h_flex().size_full().items_end().gap_1().child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.signal)
                                    .child(error),
                            ),
                        )
                        .into_any_element()
                } else {
                    let peaks = waveform.clone();
                    let grow_target = if waveform_hover.is_some() {
                        px(1.0)
                    } else {
                        px(0.0)
                    };
                    let seek_bounds = waveform_seek.then(|| waveform_bounds.clone());
                    div()
                        .relative()
                        .flex_shrink_0()
                        .w_full()
                        // The spring keeps the zoom smooth even when the pointer
                        // leaves before the row has finished growing.
                        .with_spring(
                            ("waveform-grow", id),
                            SpringAnimation::new(WAVEFORM_GROW_SPRING)
                                .to(grow_target)
                                .with_epsilon(0.004),
                            move |row, grow: gpui_kit::Pixels| {
                                let grow = f32::from(grow).clamp(0.0, 1.0);
                                let row = row
                                    .h(rems(WAVEFORM_ROW_REMS + WAVEFORM_GROW_REMS * grow))
                                    .child(waveform_band(
                                        &peaks,
                                        playing,
                                        progress,
                                        waveform_hover,
                                        grow,
                                        p,
                                    ));
                                if let Some(bounds_store) = seek_bounds.clone() {
                                    row.child(
                                        // The canvas only reports the row bounds
                                        // so a click can be turned into a clip
                                        // position.
                                        canvas(
                                            |bounds, _, _| bounds,
                                            move |bounds, _, _, _| {
                                                bounds_store.set(Some(bounds));
                                            },
                                        )
                                        .absolute()
                                        .inset_0(),
                                    )
                                } else {
                                    row
                                }
                            },
                        )
                        .into_any_element()
                }
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_shrink_0()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(if sound.volume <= 0.0 {
                                    AppIcon::VolumeX
                                } else {
                                    AppIcon::Volume2
                                })
                                .size_3()
                                .text_color(p.cream_muted),
                            )
                            .child(
                                // Padding keeps the round thumb, which is wider than
                                // the track, clear of both the speaker icon and the
                                // percentage label at the ends of its travel.
                                div().flex_1().min_w_0().px_2().when_some(
                                    volume_slider.clone(),
                                    |this, slider| {
                                        this.child(
                                            Slider::new(&slider).bg(p.blue).text_color(p.cream),
                                        )
                                    },
                                ),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.cream_muted)
                                    .child(format_percent(sound.volume)),
                            ),
                    )
                    .child(if renaming {
                        h_flex()
                            .gap_1()
                            .items_center()
                            .flex_shrink_0()
                            .child(
                                Button::new(("rename-save", id))
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(p.blue)
                                            .foreground(p.cream)
                                            .hover(p.blue_hover)
                                            .active(p.blue_active),
                                    )
                                    .small()
                                    .icon(Icon::new(AppIcon::Check))
                                    .accessibility_label("Save name")
                                    .tooltip("Save name")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.commit_rename(window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("rename-cancel", id))
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(p.console_hi)
                                            .foreground(p.cream)
                                            .hover(p.tile_hover)
                                            .active(p.tile_active),
                                    )
                                    .small()
                                    .icon(Icon::new(AppIcon::X))
                                    .accessibility_label("Cancel rename")
                                    .tooltip("Cancel rename")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_rename(window, cx);
                                    })),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .id(("tile-toggle", id))
                            .size_6()
                            .flex_shrink_0()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_sound(id, window, cx);
                            }))
                            .rounded_full()
                            .border_1()
                            .border_color(if playing { p.signal } else { p.console_line })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(tile_icon).size_3().text_color(if playing {
                                p.signal
                            } else {
                                p.cream
                            }))
                            .into_any_element()
                    }),
            );

        let interaction_layer = if renaming {
            div().into_any_element()
        } else {
            let waveform_bounds = waveform_bounds.clone();
            let cursor = if waveform_hover.is_some() {
                CursorStyle::ResizeLeftRight
            } else {
                CursorStyle::PointingHand
            };
            div()
                .id(("sound-interaction", id))
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom(px(38.0))
                .cursor(cursor)
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    let position = match event {
                        ClickEvent::Mouse(event) => Some(event.down.position),
                        ClickEvent::Touch(event) => Some(event.position),
                        ClickEvent::Keyboard(_) => None,
                    };
                    if let (Some(position), Some(bounds)) = (position, waveform_bounds.get())
                        && let Some(ratio) = super::waveform_seek_ratio(&bounds, position)
                    {
                        this.seek_from_waveform(id, ratio, window, cx);
                        return;
                    }
                    this.activate_sound(id, window, cx);
                }))
                .on_drag(TileDrag { id }, {
                    let title = title.clone();
                    move |_, _, _, cx| {
                        cx.new(|_| SoundDragPreview {
                            title: title.clone(),
                        })
                    }
                })
                .into_any_element()
        };

        // A dedicated 9x9 handle owns the top-right corner: it is the only
        // spot that shows the hand cursor, so a plain hover still reads as a
        // playable pad.
        let drag_handle = if renaming {
            div().into_any_element()
        } else {
            div()
                .id(("sound-drag", id))
                .absolute()
                .top_1()
                .right_1()
                .size_9()
                .rounded(cx.theme().radius)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|this| this.bg(p.cream.opacity(0.06)))
                .on_drag(TileDrag { id }, {
                    let title = title.clone();
                    move |_, _, _, cx| {
                        cx.new(|_| SoundDragPreview {
                            title: title.clone(),
                        })
                    }
                })
                .child(
                    Icon::new(AppIcon::Grip)
                        .size_3p5()
                        .text_color(p.cream_muted.opacity(0.7)),
                )
                .into_any_element()
        };

        let tile_body = div()
            .id(("sound-tile", id))
            .relative()
            .w_full()
            .h_full()
            .rounded(cx.theme().radius_lg)
            .bg(tile_bg)
            .border_1()
            .border_color(if selected || renaming {
                p.blue
            } else if capturing_shortcut {
                p.amber
            } else if hovered {
                p.cream_muted.opacity(0.45)
            } else {
                p.console_line
            })
            .child(content)
            .child(interaction_layer)
            .child(drag_handle)
            .into_any_element();

        let rename_disabled = self.trim_target.is_some();
        let trim_disabled = loading || failed || self.rename_target.is_some();
        let shortcut_disabled = self.trim_target.is_some() || self.rename_target.is_some();

        let wrapper = div()
            .id(("sound-wrap", id))
            .w(relative(0.32))
            .h_48()
            .flex_shrink_0()
            .on_hover(cx.listener(move |this, is_hovered, _, cx| {
                let next = if *is_hovered {
                    Some(id)
                } else if this.hovered_sound == Some(id) {
                    None
                } else {
                    this.hovered_sound
                };
                if this.hovered_sound != next {
                    this.hovered_sound = next;
                    cx.notify();
                }
                // Leaving the pad ends the preview even when the pointer exits
                // through a gap that produces no further move events.
                if !*is_hovered
                    && this
                        .waveform_hover
                        .is_some_and(|hover| hover.sound_id == id)
                {
                    this.clear_waveform_hover(cx);
                }
            }))
            .on_mouse_move(cx.listener({
                let waveform_bounds = waveform_bounds.clone();
                move |this, event: &MouseMoveEvent, _, cx| {
                    // While a button is held the pad is being dragged, not
                    // scrubbed: leave the preview where the press landed.
                    if !waveform_seek || event.pressed_button.is_some() {
                        return;
                    }
                    let ratio = waveform_bounds
                        .get()
                        .and_then(|bounds| super::waveform_seek_ratio(&bounds, event.position));
                    this.preview_waveform_hover(id, ratio, cx);
                }
            }));

        if renaming {
            return wrapper.child(tile_body).into_any_element();
        }

        wrapper
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    if this.selected_id != Some(id) {
                        this.selected_id = Some(id);
                        this.save_state();
                        cx.notify();
                    }
                }),
            )
            .drag_over::<TileDrag>(|this, _, _, _| this.opacity(0.55))
            .on_drop::<TileDrag>({
                let app = app.clone();
                move |drag, _window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.move_sound(drag.id, id, cx);
                    });
                }
            })
            .child(tile_body)
            .context_menu({
                let app = app.clone();
                move |menu, _, _| {
                    let rename = app.clone();
                    let trim = app.clone();
                    let remove = app.clone();
                    let set_shortcut = app.clone();
                    let clear_shortcut = app.clone();
                    menu.item(
                        PopupMenuItem::new(if shortcut.is_some() {
                            "Change shortcut…"
                        } else {
                            "Set shortcut…"
                        })
                        .icon(Icon::new(AppIcon::Keyboard))
                        .disabled(shortcut_disabled)
                        .on_click(move |_, window, cx| {
                            let _ = set_shortcut.update(cx, |this, cx| {
                                this.begin_shortcut_capture(id, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Clear shortcut")
                            .icon(Icon::new(AppIcon::KeyboardOff))
                            .disabled(shortcut.is_none() || shortcut_disabled)
                            .on_click(move |_, _, cx| {
                                let _ = clear_shortcut.update(cx, |this, cx| {
                                    this.clear_shortcut(id, cx);
                                });
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Rename")
                            .icon(Icon::new(AppIcon::Pencil))
                            .disabled(rename_disabled)
                            .on_click(move |_, window, cx| {
                                let _ = rename.update(cx, |this, cx| {
                                    this.begin_rename(id, window, cx);
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Trim")
                            .icon(Icon::new(AppIcon::Scissors))
                            .disabled(trim_disabled)
                            .on_click(move |_, window, cx| {
                                let _ = trim.update(cx, |this, cx| {
                                    this.open_trim_for(id, window, cx);
                                });
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Delete")
                            .icon(Icon::new(AppIcon::Trash))
                            .on_click(move |_, window, cx| {
                                let _ = remove.update(cx, |this, cx| {
                                    this.remove_sound(id, window, cx);
                                });
                            }),
                    )
                }
            })
            .into_any_element()
    }

    fn render_routing(&mut self, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        let output_ready = self.output_device.is_some();
        let output_virtual = self.output_is_virtual();
        let input_ready = self.input_device.is_some();
        let mic_level = normalize_peak(self.mic_level);
        let peak_db = peak_to_db(self.output_level_l.max(self.output_level_r));
        let peak_label = peak_db
            .map(|db| format!("{db:>5.1}"))
            .unwrap_or_else(|| "  −∞  ".to_string());

        let refresh = Button::new("refresh-devices")
            .icon(Icon::new(AppIcon::RefreshCw))
            .small()
            .ghost()
            .accessibility_label("Refresh audio devices")
            .tooltip("Refresh audio devices")
            .on_click(cx.listener(|this, _, window, cx| {
                this.refresh_devices(window, cx);
            }));

        let output_select = Select::new(&self.output_select)
            .w_full()
            .accessibility_label("Audio output")
            .icon(Icon::new(AppIcon::Cable))
            .placeholder("No output")
            .disabled(self.output_devices.is_empty());

        let input_select = Select::new(&self.input_select)
            .w_full()
            .accessibility_label("Input microphone")
            .icon(Icon::new(AppIcon::Mic))
            .placeholder("No microphone")
            .disabled(self.input_devices.is_empty());

        let monitor_select = Select::new(&self.monitor_select)
            .w_full()
            .accessibility_label("Headphone monitor")
            .icon(Icon::new(AppIcon::Headphones))
            .placeholder("No monitor")
            .disabled(self.output_device.is_none());

        let (route_label, route_color, route_hint) = if !output_ready {
            ("NO OUTPUT", p.signal, "Connect a device and press refresh.")
        } else if output_virtual {
            (
                "VIRTUAL MICROPHONE",
                p.green,
                "In Discord, select the cable's Output side as your microphone.",
            )
        } else {
            (
                "DIRECT OUTPUT",
                p.amber,
                "Choose a virtual cable to send sounds to the microphone.",
            )
        };

        let output_block = div()
            .v_flex()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(field_label("SOUND OUTPUT", p.ink_muted))
                    .child(refresh),
            )
            .child(output_select)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size_1p5().rounded_full().bg(route_color))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(route_color)
                            .child(route_label),
                    ),
            )
            .when(!output_virtual, |this| {
                this.child(
                    div()
                        .text_xs()
                        .line_height(relative(1.35))
                        .text_color(p.ink_muted)
                        .child(route_hint),
                )
            });

        let input_block = div()
            .v_flex()
            .gap_1()
            .child(field_label("VOICE INPUT", p.ink_muted))
            .child(input_select)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(if input_ready { p.ink_muted } else { p.signal })
                            .child(if input_ready {
                                self.input_summary()
                            } else {
                                SharedString::from("No input")
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.ink_muted)
                                    .child("SIGNAL"),
                            )
                            .child(
                                div().w_16().h_1().rounded_full().bg(p.line).child(
                                    div()
                                        .h_full()
                                        .w(relative(mic_level.clamp(0.0, 1.0)))
                                        .rounded_full()
                                        .bg(p.green),
                                ),
                            ),
                    ),
            );

        let monitor_active = self.monitor_device.is_some();
        let monitor_block = div()
            .v_flex()
            .gap_1()
            .child(field_label("HEADPHONE MONITOR", p.ink_muted))
            .child(monitor_select)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(p.ink_muted)
                            .child("HEAR THE MIX IN HEADPHONES"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(div().size_1p5().rounded_full().bg(if monitor_active {
                                p.green
                            } else {
                                p.line_strong
                            }))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(if monitor_active { p.green } else { p.ink_muted })
                                    .child(if monitor_active { "ON" } else { "OFF" }),
                            ),
                    ),
            );

        let voice_mix = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(p.line)
            .pt_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_0()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.ink)
                            .child("Mix my voice"),
                    )
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .line_height(relative(1.25))
                            .text_color(p.ink_muted)
                            .child("Voice + sounds on one channel"),
                    ),
            )
            .child(
                div().flex_shrink_0().child(
                    Switch::new("voice-mix")
                        .checked(self.mic_monitor)
                        .disabled(!input_ready)
                        .accessibility_label("Mix the microphone with the sounds")
                        .on_change(cx.listener(|this, checked, _, cx| {
                            this.toggle_mic_monitor(*checked, cx);
                        })),
                ),
            );

        let plate = div()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(p.line)
            .bg(p.paper_hi)
            .p_3()
            .v_flex()
            .gap_2()
            .child(output_block)
            .child(div().h(px(1.0)).w_full().bg(p.line))
            .child(input_block)
            .child(monitor_block)
            .child(voice_mix);

        let error_note = self.audio_error.as_ref().map(|error| {
            div()
                .rounded(cx.theme().radius)
                .bg(p.signal_soft)
                .border_1()
                .border_color(p.signal.opacity(0.35))
                .px_3()
                .py_2()
                .text_xs()
                .line_height(relative(1.4))
                .text_color(p.ink)
                .child(error.clone())
        });

        let meter_card = div()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(p.console_line)
            .bg(p.meter_face)
            .p_3()
            .v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(p.cream_muted)
                            .child("OUTPUT LEVEL"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.amber)
                                    .child(peak_label),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.cream_muted)
                                    .child("dB"),
                            ),
                    ),
            )
            .child(vu_meter(self.output_level_l, self.output_level_r))
            .child(
                h_flex()
                    .justify_between()
                    .px_1()
                    .font_family(MONO_FONT)
                    .text_xs()
                    .text_color(p.cream_muted.opacity(0.55))
                    .children(["-48", "-24", "-12", "-6", "0"].map(|label| div().child(label))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .border_t_1()
                    .border_color(p.console_line)
                    .pt_2()
                    .child(legend_dot(p.amber, "L"))
                    .child(legend_dot(p.green, "R"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(p.cream_muted)
                            .child(if output_virtual {
                                "ROUTING ACTIVE"
                            } else {
                                "MONITOR ONLY"
                            }),
                    ),
            );

        div()
            .w_80()
            .h_full()
            .flex_shrink_0()
            .min_h_0()
            .bg(p.console)
            .border_l_1()
            .border_color(p.console_line)
            .child(
                div()
                    .id("routing-scroll")
                    .h_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .p_4()
                            .flex()
                            .flex_col()
                            .min_h_full()
                            .gap_3()
                            .when_some(error_note, |this, note| this.child(note))
                            .child(plate)
                            .child(meter_card),
                    ),
            )
    }

    fn render_transport(&mut self, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        let selected = self.selected_sound();
        let title = selected
            .map(|sound| sound.title.clone())
            .unwrap_or_else(|| SharedString::from("No sound selected"));
        let subtitle = selected
            .map(|sound| {
                format!(
                    "{} · {} · {}",
                    sound.format_label(),
                    format_duration(sound.duration()),
                    sound.location_label()
                )
            })
            .unwrap_or_else(|| "Choose a pad from the bank to start".to_string());

        let current_dot = if self.is_playing() {
            p.signal
        } else if self.transport.is_some() {
            p.amber
        } else {
            p.cream_muted.opacity(0.45)
        };

        let prev = console_icon_button(
            cx,
            "transport-prev",
            AppIcon::SkipBack,
            "Previous track",
            !self.sounds.is_empty(),
        )
        .on_click(cx.listener(|this, _, window, cx| this.previous(window, cx)));

        let next = console_icon_button(
            cx,
            "transport-next",
            AppIcon::SkipForward,
            "Next track",
            !self.sounds.is_empty(),
        )
        .on_click(cx.listener(|this, _, window, cx| this.skip(1, window, cx)));

        let stop = console_icon_button(
            cx,
            "transport-stop",
            AppIcon::Square,
            "Stop · Esc",
            self.transport.is_some(),
        )
        .on_click(cx.listener(|this, _, window, cx| this.stop_playback(window, cx)));

        let play_icon = if self.is_playing() {
            AppIcon::Pause
        } else {
            AppIcon::Play
        };
        let play_label = if self.is_playing() { "Pause" } else { "Play" };
        let play = Button::new("transport-play")
            .primary()
            .size_10()
            .p_0()
            .rounded_full()
            .accessibility_label(format!("{play_label} the selected track"))
            .tooltip(format!("{play_label} · Ctrl+P"))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_playback(window, cx)))
            .child(Icon::new(play_icon).size_4().text_color(p.cream));

        let loop_color = if self.looping { p.amber } else { p.cream_muted };
        let loop_button = Button::new("transport-loop")
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(if self.looping {
                        p.console_hi
                    } else {
                        p.console
                    })
                    .foreground(loop_color)
                    .hover(p.console_hi)
                    .active(p.console_line),
            )
            .size_8()
            .p_0()
            .rounded(cx.theme().radius)
            .icon(Icon::new(AppIcon::Repeat2).size_4().text_color(loop_color))
            .accessibility_label("Loop the selected track")
            .tooltip("Loop the track · Ctrl+L")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_loop(cx)));

        let mute_icon = if self.muted || self.master_volume <= 0.001 {
            AppIcon::VolumeX
        } else {
            AppIcon::Volume2
        };
        let mute = console_icon_button(
            cx,
            "transport-mute",
            mute_icon,
            if self.muted { "Unmute" } else { "Mute" },
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)));

        let volume_slider = Slider::new(&self.master_slider)
            .bg(p.amber)
            .text_color(p.cream)
            .accessibility_label("Master volume");
        let seek_slider = Slider::new(&self.seek_slider)
            .bg(p.blue)
            .text_color(p.cream)
            .accessibility_label("Track position")
            .disabled(self.selected_duration() <= 0.0);

        let controls = h_flex()
            .justify_center()
            .items_center()
            .gap_2()
            .child(prev)
            .child(play)
            .child(stop)
            .child(next)
            .child(div().w_2())
            .child(loop_button);

        let timeline = h_flex()
            .w_full()
            .max_w_128()
            .items_center()
            .gap_3()
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_xs()
                    .text_color(p.cream_muted)
                    .child(self.playhead_time()),
            )
            .child(div().flex_1().min_w_0().child(seek_slider))
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_xs()
                    .text_color(p.cream_muted)
                    .child(format_duration(self.selected_duration())),
            );

        let volume = h_flex()
            .w_56()
            .flex_shrink_0()
            .items_center()
            .justify_end()
            .gap_3()
            .child(mute)
            .child(
                div()
                    .v_flex()
                    .gap_0()
                    .flex_1()
                    .min_w_0()
                    .max_w_32()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.cream_muted)
                                    .child("MASTER"),
                            )
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(if self.muted { p.amber } else { p.cream_muted })
                                    .child(self.volume_summary()),
                            ),
                    )
                    .child(volume_slider),
            );

        h_flex()
            .h_24()
            .flex_shrink_0()
            .w_full()
            .px_5()
            .gap_5()
            .items_center()
            .bg(p.console)
            .border_t_1()
            .border_color(p.console_line)
            .child(
                div()
                    .w_64()
                    .flex_shrink_0()
                    .min_w_0()
                    .v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size_1p5().rounded_full().bg(current_dot))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_xs()
                                    .text_color(p.cream_muted)
                                    .child(if self.is_playing() {
                                        "ON AIR"
                                    } else if self.transport.is_some() {
                                        "PAUSED"
                                    } else {
                                        "READY"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(DISPLAY_FONT)
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.cream)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO_FONT)
                            .text_xs()
                            .text_color(p.cream_muted)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(controls)
                    .child(timeline),
            )
            .child(volume)
    }
}

#[derive(Clone, Copy)]
enum WindowControlAction {
    Minimize,
    Maximize,
    Close,
}

fn window_control_button(
    cx: &mut gpui_kit::Context<SoundboardApp>,
    id: &'static str,
    icon: AppIcon,
    hover_bg: Hsla,
    active_bg: Hsla,
    accessibility_label: &'static str,
    action: WindowControlAction,
) -> Button {
    let p = palette();
    Button::new(id)
        .icon(Icon::new(icon).size_3p5().text_color(p.cream))
        .custom(
            ButtonCustomVariant::new(cx)
                .color(p.console)
                .foreground(p.cream)
                .hover(hover_bg)
                .active(active_bg),
        )
        .w_12()
        .h_full()
        .p_0()
        .rounded(px(0.0))
        .tab_stop(false)
        .accessibility_label(accessibility_label)
        .on_click(move |_, window, _| match action {
            WindowControlAction::Minimize => window.minimize_window(),
            WindowControlAction::Maximize => toggle_window_expanded(window),
            WindowControlAction::Close => window.remove_window(),
        })
}

fn toggle_window_expanded(window: &Window) {
    if window.is_fullscreen() {
        window.toggle_fullscreen();
    } else if window.is_maximized() {
        restore_window(window);
    } else {
        window.zoom_window();
    }
}

#[cfg(windows)]
fn restore_window(window: &Window) {
    use raw_window_handle::RawWindowHandle;

    const SW_RESTORE: i32 = 9;
    const WM_SIZE: u32 = 0x0005;

    #[repr(C)]
    struct ClientRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindow(hwnd: *mut std::ffi::c_void, command: i32) -> i32;
        fn GetClientRect(hwnd: *mut std::ffi::c_void, rect: *mut ClientRect) -> i32;
        fn PostMessageW(
            hwnd: *mut std::ffi::c_void,
            message: u32,
            wparam: usize,
            lparam: isize,
        ) -> i32;
    }

    if let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        let hwnd = win32.hwnd.get() as *mut std::ffi::c_void;
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            let mut rect = ClientRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(hwnd, &mut rect) != 0 {
                let width = (rect.right - rect.left).max(1);
                let height = (rect.bottom - rect.top).max(1);
                let lparam = ((height as isize) << 16) | (width as isize & 0xFFFF);
                // GPUI ignores the resize produced while the window is being
                // restored from the click handler, so ask for one more pass
                // after the current event has been processed.
                PostMessageW(hwnd, WM_SIZE, 0, lparam);
            }
        }
    }
}

#[cfg(not(windows))]
fn restore_window(_: &Window) {}

#[derive(Clone)]
struct TileDrag {
    id: SoundId,
}

struct SoundDragPreview {
    title: SharedString,
}

impl Render for SoundDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        let p = palette();
        h_flex()
            .w_56()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .rounded(px(10.0))
            .bg(p.console_hi)
            .border_1()
            .border_color(p.blue)
            .shadow_md()
            .child(
                Icon::new(AppIcon::GripVertical)
                    .size_4()
                    .text_color(p.cream_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(DISPLAY_FONT)
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.cream)
                    .child(self.title.clone()),
            )
    }
}

fn peak_bars(samples: &[f32], bars: usize) -> Vec<f32> {
    let frames = samples.len() / 2;
    if frames == 0 || bars == 0 {
        return Vec::new();
    }
    let mut peaks = Vec::with_capacity(bars);
    for bar in 0..bars {
        let start = bar * frames / bars;
        let end = ((bar + 1) * frames / bars).max(start + 1).min(frames);
        let stride = ((end - start) / 64).max(1);
        let mut peak = 0.0f32;
        for frame in (start..end).step_by(stride) {
            peak = peak
                .max(samples[frame * 2].abs())
                .max(samples[frame * 2 + 1].abs());
        }
        peaks.push(peak.sqrt().clamp(0.0, 1.0));
    }
    peaks
}

fn section_label(label: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .font_family(MONO_FONT)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
}

fn field_label(label: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .font_family(MONO_FONT)
        .text_xs()
        .text_color(color)
        .child(label)
}

fn console_icon_button(
    cx: &App,
    id: &'static str,
    icon: AppIcon,
    tooltip: &'static str,
    enabled: bool,
) -> Button {
    let p = palette();
    Button::new(id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(p.console)
                .foreground(if enabled {
                    p.cream
                } else {
                    p.cream_muted.opacity(0.45)
                })
                .hover(p.console_hi)
                .active(p.console_line),
        )
        .size_8()
        .p_0()
        .rounded(cx.theme().radius)
        .icon(Icon::new(icon).size_4().text_color(if enabled {
            p.cream
        } else {
            p.cream_muted.opacity(0.45)
        }))
        .accessibility_label(tooltip)
        .tooltip(tooltip)
        .disabled(!enabled)
}

fn legend_dot(color: Hsla, label: &'static str) -> impl IntoElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(div().size_1p5().rounded_full().bg(color))
        .child(
            div()
                .font_family(MONO_FONT)
                .text_xs()
                .text_color(color)
                .child(label),
        )
}

fn placeholder_waveform() -> Vec<f32> {
    (0..28)
        .map(|index| 0.2 + ((index as f32 * 0.7).sin().abs() * 0.5))
        .collect()
}

/// Height of the pad waveform band at rest (`h_6`) and the extra height the
/// hover zoom adds to it.
const WAVEFORM_ROW_REMS: f32 = 1.5;
const WAVEFORM_GROW_REMS: f32 = 1.1;

/// Critically damped spring behind the hover zoom: about a fifth of a second
/// to settle, without a bounce that would outgrow the free space in the pad.
const WAVEFORM_GROW_SPRING: SpringConfig = SpringConfig::new(900.0, 60.0, 1.0);

/// The pad waveform band.
///
/// `grow` scales the band from its resting size (0) to the hovered size (1),
/// and `hover` is the position under the pointer: the bars up to it light up
/// in the seek colour and a playhead marks the exact point a click would hit.
fn waveform_band(
    peaks: &[f32],
    playing: bool,
    progress: f32,
    hover: Option<f32>,
    grow: f32,
    p: &'static Palette,
) -> impl IntoElement {
    let count = peaks.len();
    let idle_alpha = if playing {
        0.28 - 0.06 * grow
    } else {
        0.45 - 0.12 * grow
    };
    let preview_alpha = 0.55 + 0.45 * grow;
    let height_scale = 1.0 + 0.62 * grow;
    h_flex()
        .relative()
        .size_full()
        .items_end()
        .gap_1()
        .children(peaks.iter().enumerate().map(move |(bar, peak)| {
            let played = super::waveform_bar_filled(bar, count, progress);
            let previewed =
                hover.is_some_and(|ratio| super::waveform_bar_filled(bar, count, ratio));
            let color = if playing && played {
                p.signal
            } else if previewed {
                p.blue.opacity(preview_alpha)
            } else {
                p.cream_muted.opacity(idle_alpha)
            };
            div()
                .flex_1()
                .min_w(px(1.0))
                .h(rems((0.2 + peak.clamp(0.0, 1.0) * 1.4) * height_scale))
                .rounded_full()
                .bg(color)
        }))
        .when_some(hover, move |this, ratio| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(ratio))
                    .ml(px(-1.0))
                    .w(px(2.0))
                    .rounded_full()
                    .bg(p.cream.opacity(0.45 + 0.55 * grow)),
            )
        })
}

/// A compact analog VU face. The scale, tick rails and both needles are painted
/// as vectors so the visual stays crisp at every display scale.
fn vu_meter(level_l: f32, level_r: f32) -> impl IntoElement {
    let p = palette();
    let l = normalize_peak(level_l);
    let r = normalize_peak(level_r);
    let tick_major = p.cream_muted.opacity(0.58);
    let tick_minor = p.cream_muted.opacity(0.25);
    let tick_hot = p.signal.opacity(0.75);
    let arc = p.cream_muted.opacity(0.28);
    let center_color = p.cream;
    let needle_l = p.amber;
    let needle_r = p.green;

    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, cx| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let origin_x = f32::from(bounds.origin.x);
            let origin_y = f32::from(bounds.origin.y);

            // A shallow instrument arc with its pivot just below the visible face.
            let pivot = point(px(origin_x + width * 0.5), px(origin_y + height * 0.91));
            let radius = width.min(height * 1.6) * 0.43;
            let start = 200.0_f32.to_radians();
            let sweep = 140.0_f32.to_radians();
            let at = |t: f32| {
                let angle = start + sweep * t.clamp(0.0, 1.0);
                point(
                    px(origin_x + width * 0.5 + radius * angle.cos()),
                    px(origin_y + height * 0.91 + radius * angle.sin()),
                )
            };

            let mut arc_builder = PathBuilder::stroke(px(1.0));
            for step in 0..=48 {
                let point = at(step as f32 / 48.0);
                if step == 0 {
                    arc_builder.move_to(point);
                } else {
                    arc_builder.line_to(point);
                }
            }
            if let Ok(path) = arc_builder.build() {
                window.paint_path(path, arc);
            }

            let ticks = 41;
            for index in 0..=ticks {
                let t = index as f32 / ticks as f32;
                let angle = start + sweep * t;
                let outer = radius;
                let inner = radius
                    - if index % 5 == 0 { 10.0 } else { 5.0 }
                    - if t > 0.88 { 2.0 } else { 0.0 };
                let outer_point = point(
                    px(origin_x + width * 0.5 + outer * angle.cos()),
                    px(origin_y + height * 0.91 + outer * angle.sin()),
                );
                let inner_point = point(
                    px(origin_x + width * 0.5 + inner * angle.cos()),
                    px(origin_y + height * 0.91 + inner * angle.sin()),
                );
                let mut tick = PathBuilder::stroke(px(if index % 5 == 0 { 1.4 } else { 1.0 }));
                tick.move_to(outer_point);
                tick.line_to(inner_point);
                if let Ok(path) = tick.build() {
                    window.paint_path(
                        path,
                        if t > 0.88 {
                            tick_hot
                        } else if index % 5 == 0 {
                            tick_major
                        } else {
                            tick_minor
                        },
                    );
                }
            }

            let mut needle = |level: f32, color: Hsla, length: f32, width_px: f32| {
                let angle = start + sweep * level.clamp(0.0, 1.0);
                let end = point(
                    px(origin_x + width * 0.5 + radius * length * angle.cos()),
                    px(origin_y + height * 0.91 + radius * length * angle.sin()),
                );
                let mut path = PathBuilder::stroke(px(width_px));
                path.move_to(pivot);
                path.line_to(end);
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            needle(r, needle_r, 0.84, 1.4);
            needle(l, needle_l, 0.94, 2.0);

            window.paint_quad(quad(
                gpui_kit::Bounds {
                    origin: point(
                        px(origin_x + width * 0.5 - 4.0),
                        px(origin_y + height * 0.91 - 4.0),
                    ),
                    size: gpui_kit::size(px(8.0), px(8.0)),
                },
                cx.theme().radius_full(),
                center_color,
                px(0.0),
                transparent_black(),
                Default::default(),
            ));
        },
    )
    .w_full()
    .h_32()
}
