#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod lyrics;
mod media;
mod tray;
mod win;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, ModelRc, SharedString, Timer, TimerMode, VecModel};
use windows::Win32::Foundation::HWND;

use config::Config;
use media::{Command, MediaController, MediaState, Update};

slint::include_modules!();

/// Everything the UI thread owns. Lives in a thread-local so callbacks coming
/// from other threads (via `invoke_from_event_loop`) can reach it.
struct App {
    ui: Island,
    hwnd: Option<HWND>,
    cfg: Config,
    media: MediaController,
    tray: Option<tray::Tray>,

    state: MediaState,
    base_pos: i64,
    base_at: Instant,
    ticker: Timer,

    lyrics: Vec<lyrics::Line>,
    lyrics_synced: bool,
    lyrics_gen: u64,
    lyric_index: i32,

    region: (f32, f32),
    region_gen: u64,
    fullscreen: bool,
    shown: bool,
    absent_gen: u64,
    peek_gen: u64,
    last_fg: win::Foreground,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state. If the state is already borrowed (a Win32 or
/// Slint callback re-entered us), the call is deferred to the next loop turn.
fn with_app(f: impl FnOnce(&mut App) + 'static) {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                f(app);
            }
        }
        Err(_) => Timer::single_shot(Duration::ZERO, move || with_app(f)),
    });
}

fn format_time(ms: i32) -> SharedString {
    let s = (ms.max(0) / 1000) as u32;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60).into()
    } else {
        format!("{}:{:02}", s / 60, s % 60).into()
    }
}

fn source_name(app_id: &str) -> String {
    let id = app_id.to_ascii_lowercase();
    let known = [
        ("spotify", "Spotify"),
        ("yandex", "Яндекс Музыка"),
        ("chrome", "Google Chrome"),
        ("msedge", "Microsoft Edge"),
        ("firefox", "Firefox"),
        ("opera", "Opera"),
        ("zune", "Media Player"),
        ("applemusic", "Apple Music"),
        ("vlc", "VLC"),
        ("aimp", "AIMP"),
        ("foobar", "foobar2000"),
    ];
    for (needle, name) in known {
        if id.contains(needle) {
            return name.to_string();
        }
    }
    let base = app_id.split('!').next().unwrap_or(app_id);
    base.trim_end_matches(".exe").trim_end_matches(".EXE").to_string()
}

impl App {
    fn apply_settings(&self) {
        let u = &self.cfg.ui;
        self.ui.set_visualizer(u.visualizer);
        self.ui.set_compact_lyrics(u.compact_lyrics);
        self.ui.set_expand_on_hover(u.expand_on_hover);
        self.ui.set_top_offset(u.top_offset.max(0) as f32);
    }

    fn update_visibility(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let hidden_fs = self.cfg.ui.hide_in_fullscreen && self.fullscreen;
        let idle_hidden = self.cfg.ui.hide_when_idle && !self.state.present;
        let visible = !hidden_fs && !idle_hidden;
        if visible && !self.shown {
            self.repaint_all();
        }
        self.shown = visible;
        win::show(hwnd, visible);
    }

    fn apply_region(&self, (w, h): (f32, f32)) {
        if let Some(hwnd) = self.hwnd {
            let scale = self.ui.window().scale_factor();
            win::set_hit_region(hwnd, scale, w, h, self.ui.get_pill_top());
            self.repaint_all();
        }
    }

    fn on_foreground(&mut self, fg: win::Foreground) {
        self.last_fg = fg;
        self.fullscreen = fg.fullscreen;
        self.ui.set_tucked(self.cfg.ui.auto_tuck && fg.under_island);
        self.update_visibility();
    }

    /// Shows the compact island for a few seconds even while tucked.
    fn peek(&mut self) {
        self.peek_gen += 1;
        let gen = self.peek_gen;
        self.ui.set_peek(true);
        Timer::single_shot(Duration::from_millis(3500), move || {
            with_app(move |app| {
                if app.peek_gen == gen {
                    app.ui.set_peek(false);
                }
            })
        });
    }

    fn repaint_all(&self) {
        self.ui.set_repaint_all(!self.ui.get_repaint_all());
    }

    /// Grow the hit-region immediately, shrink it only after the collapse
    /// animation has finished, so the morph is never clipped.
    fn on_shape_changed(&mut self) {
        let target = (self.ui.get_pill_width(), self.ui.get_pill_height());
        let grown = (self.region.0.max(target.0), self.region.1.max(target.1));
        self.region = grown;
        self.apply_region(grown);
        self.region_gen += 1;
        let gen = self.region_gen;
        Timer::single_shot(Duration::from_millis(650), move || {
            with_app(move |app| {
                if app.region_gen == gen {
                    app.region = target;
                    app.apply_region(target);
                }
            })
        });
    }

    fn current_pos(&self) -> i64 {
        let mut pos = self.base_pos;
        if self.state.playing {
            pos += self.base_at.elapsed().as_millis() as i64;
        }
        if self.state.duration_ms > 0 {
            pos = pos.min(self.state.duration_ms);
        }
        pos.max(0)
    }

    fn tick(&mut self) {
        let pos = self.current_pos();
        self.ui.set_position_ms(pos as i32);
        if self.lyrics_synced && !self.lyrics.is_empty() {
            let t = pos + self.cfg.lyrics.offset_ms;
            let idx = self.lyrics.partition_point(|l| l.time <= t) as i32 - 1;
            if idx != self.lyric_index {
                self.lyric_index = idx;
                self.ui.set_lyric_index(idx);
            }
        }
    }

    fn restart_ticker(&mut self) {
        if self.state.present && self.state.playing {
            if !self.ticker.running() {
                self.ticker.start(TimerMode::Repeated, Duration::from_millis(120), || with_app(App::tick));
            }
        } else {
            self.ticker.stop();
        }
        self.tick();
    }

    fn on_media(&mut self, update: Update) {
        match update {
            Update::State(state, at) => self.on_state(state, at),
            Update::Art(Some(art)) => {
                let [r, g, b] = art.accent;
                self.ui.set_art(Image::from_rgba8(art.pixels));
                self.ui.set_has_art(true);
                self.ui.set_accent(slint::Color::from_rgb_u8(r, g, b));
            }
            Update::Art(None) => {
                self.ui.set_has_art(false);
                self.ui.set_accent(slint::Color::from_rgb_u8(0x9b, 0xe7, 0xb4));
            }
        }
    }

    /// Players briefly drop their session or publish an empty title while
    /// switching tracks. Wait a moment before treating that as "nothing is
    /// playing", so the island doesn't collapse or flicker on every skip.
    fn on_state(&mut self, state: MediaState, at: Instant) {
        self.absent_gen += 1;
        let blank = !state.present || (state.title.is_empty() && state.artist.is_empty());
        if blank && self.state.present {
            let gen = self.absent_gen;
            Timer::single_shot(Duration::from_millis(1500), move || {
                with_app(move |app| {
                    if app.absent_gen == gen {
                        app.apply_state(state, at);
                    }
                })
            });
            return;
        }
        self.apply_state(state, at);
    }

    fn apply_state(&mut self, state: MediaState, at: Instant) {
        let track_changed = state.track_key() != self.state.track_key() || state.present != self.state.present;
        if track_changed && state.present && self.state.present {
            self.peek();
        }
        let ui = &self.ui;
        ui.set_has_media(state.present);
        ui.set_playing(state.playing);
        ui.set_track_title(state.title.clone().into());
        ui.set_track_artist(state.artist.clone().into());
        ui.set_source_name(if state.present { source_name(&state.app_id).into() } else { "".into() });
        ui.set_duration_ms(state.duration_ms as i32);
        ui.set_can_seek(state.can_seek);
        ui.set_can_next(state.can_next);
        ui.set_can_prev(state.can_prev);

        self.base_pos = state.position_ms;
        self.base_at = at;
        self.state = state;

        if track_changed {
            self.load_lyrics();
        }
        self.restart_ticker();
        self.update_visibility();
    }

    fn set_lyrics(&mut self, lines: Vec<lyrics::Line>, synced: bool, status: &str, source: &str) {
        let model: Vec<LyricLine> = lines
            .iter()
            .map(|l| LyricLine { text: l.text.clone().into(), time: l.time.clamp(-1, i32::MAX as i64) as i32 })
            .collect();
        self.lyrics = lines;
        self.lyrics_synced = synced;
        self.lyric_index = -1;
        self.ui.set_lyric_index(-1);
        self.ui.set_lyrics_synced(synced);
        self.ui.set_lyrics(ModelRc::from(Rc::new(VecModel::from(model))));
        self.ui.set_lyrics_status(status.into());
        self.ui.set_lyrics_source(source.into());
        self.tick();
    }

    fn load_lyrics(&mut self) {
        self.lyrics_gen += 1;
        let gen = self.lyrics_gen;
        if !self.state.present || self.state.title.trim().is_empty() {
            self.set_lyrics(Vec::new(), false, "", "");
            return;
        }
        self.set_lyrics(Vec::new(), false, "Ищу текст…", "");

        let query = lyrics::Query {
            title: self.state.title.clone(),
            artist: self.state.artist.clone(),
            album: self.state.album.clone(),
            duration_ms: self.state.duration_ms,
            app_id: self.state.app_id.clone(),
        };
        let cfg = self.cfg.lyrics.clone();
        std::thread::Builder::new()
            .name("lyrics".into())
            .spawn(move || {
                let result = lyrics::fetch(&query, &cfg);
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(move |app| {
                        if app.lyrics_gen != gen {
                            return; // track changed meanwhile
                        }
                        match result {
                            Some(l) => app.set_lyrics(l.lines, l.synced, "", &l.source),
                            None => app.set_lyrics(Vec::new(), false, "Текст не найден", ""),
                        }
                    })
                });
            })
            .ok();
    }

    fn on_tray(&mut self, action: tray::Action) {
        match action {
            tray::Action::OpenConfig => {
                let _ = std::process::Command::new("notepad.exe").arg(config::config_path()).spawn();
            }
            tray::Action::ReloadConfig => {
                self.cfg = Config::load();
                self.apply_settings();
                self.on_foreground(self.last_fg);
                self.on_shape_changed();
                self.load_lyrics();
            }
            tray::Action::ClearLyricsCache => {
                lyrics::clear_cache();
                self.load_lyrics();
            }
            tray::Action::ToggleAutostart => {
                let enable = !win::autostart_enabled();
                win::set_autostart(enable);
                if let Some(t) = &self.tray {
                    t.autostart.set_checked(win::autostart_enabled());
                }
            }
            tray::Action::Quit => {
                let _ = slint::quit_event_loop();
            }
        }
    }
}

/// The native window is created lazily once the event loop runs, so wait for
/// it before applying Win32 styles.
fn init_native_window() {
    let timer = Rc::new(Timer::default());
    let t = timer.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(5), move || {
        let hwnd = APP.with(|a| a.try_borrow().ok()?.as_ref().and_then(|app| win::hwnd_of(app.ui.window())));
        let Some(hwnd) = hwnd else { return };
        t.stop();
        with_app(move |app| {
            app.hwnd = Some(hwnd);
            win::make_overlay(hwnd);
            win::place_top_center(hwnd);
            app.region = (app.ui.get_pill_width(), app.ui.get_pill_height());
            app.apply_region(app.region);
            app.update_visibility();
            win::watch_foreground(hwnd, |fg| with_app(move |app| app.on_foreground(fg)));
        });
    });
    std::mem::forget(timer);
}

fn main() -> Result<(), slint::PlatformError> {
    if !win::single_instance() {
        return Ok(());
    }

    let cfg = Config::load();
    let ui = Island::new()?;

    ui.on_format_time(format_time);
    ui.on_play_pause(|| with_app(|a| a.media.send(Command::PlayPause)));
    ui.on_next(|| with_app(|a| a.media.send(Command::Next)));
    ui.on_prev(|| with_app(|a| a.media.send(Command::Prev)));
    ui.on_seek(|ms| {
        with_app(move |a| {
            // Optimistic update so the bar doesn't jump back while the player catches up.
            a.base_pos = ms as i64;
            a.base_at = Instant::now();
            a.tick();
            a.media.send(Command::Seek(ms as i64));
        })
    });
    ui.on_shape_changed(|| with_app(App::on_shape_changed));

    let media = media::spawn(|update| {
        // Art pixel buffers are Send; `Image` is built on the UI thread.
        let _ = slint::invoke_from_event_loop(move || with_app(|a| a.on_media(update)));
    });

    let tray = tray::create(win::autostart_enabled(), |action| {
        let _ = slint::invoke_from_event_loop(move || with_app(move |a| a.on_tray(action)));
    });

    let app = App {
        ui: ui.clone_strong(),
        hwnd: None,
        cfg,
        media,
        tray: Some(tray),
        state: MediaState::default(),
        base_pos: 0,
        base_at: Instant::now(),
        ticker: Timer::default(),
        lyrics: Vec::new(),
        lyrics_synced: false,
        lyrics_gen: 0,
        lyric_index: -1,
        region: (0.0, 0.0),
        region_gen: 0,
        fullscreen: false,
        shown: false,
        absent_gen: 0,
        peek_gen: 0,
        last_fg: win::Foreground::default(),
    };
    app.apply_settings();
    APP.with(|a| *a.borrow_mut() = Some(app));

    ui.show()?;
    init_native_window();
    slint::run_event_loop_until_quit()?;

    APP.with(|a| a.borrow_mut().take());
    Ok(())
}
