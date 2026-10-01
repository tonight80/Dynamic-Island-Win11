//! Now-playing state and transport control through Windows GSMTC
//! (Global System Media Transport Controls). Spotify, Yandex Music, browsers
//! and most other players publish their sessions there.
//!
//! Everything runs on one background thread that sleeps on a channel: GSMTC
//! events wake it up, so there is no polling.

use std::sync::mpsc::{self, Sender};
use std::time::Instant;

use slint::{Rgba8Pixel, SharedPixelBuffer};
use windows::core::Result;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

#[derive(Debug, Clone, Copy)]
pub enum Command {
    PlayPause,
    Next,
    Prev,
    Seek(i64),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaState {
    pub present: bool,
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub can_seek: bool,
    pub can_next: bool,
    pub can_prev: bool,
}

impl MediaState {
    pub fn track_key(&self) -> String {
        format!("{}\u{1f}{}", self.artist, self.title)
    }
}

pub struct Art {
    pub pixels: SharedPixelBuffer<Rgba8Pixel>,
    pub accent: [u8; 3],
}

pub enum Update {
    /// New state plus the instant its `position_ms` was valid.
    State(MediaState, Instant),
    Art(Option<Art>),
}

enum Msg {
    Refresh,
    Cmd(Command),
}

pub struct MediaController {
    tx: Sender<Msg>,
}

impl MediaController {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }
}

/// Starts the media thread. `on_update` is called from that thread.
pub fn spawn(on_update: impl Fn(Update) + Send + 'static) -> MediaController {
    let (tx, rx) = mpsc::channel::<Msg>();
    let thread_tx = tx.clone();
    std::thread::Builder::new()
        .name("media".into())
        .spawn(move || {
            let mut worker = match Worker::new(thread_tx, on_update) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("GSMTC unavailable: {e}");
                    return;
                }
            };
            worker.refresh();
            while let Ok(msg) = rx.recv() {
                // Collapse bursts of events (players often fire 3-4 at once).
                let mut refresh = matches!(msg, Msg::Refresh);
                if let Msg::Cmd(c) = msg {
                    worker.command(c);
                }
                while let Ok(m) = rx.try_recv() {
                    match m {
                        Msg::Refresh => refresh = true,
                        Msg::Cmd(c) => worker.command(c),
                    }
                }
                if refresh {
                    worker.refresh();
                }
            }
        })
        .expect("spawn media thread");
    MediaController { tx }
}

struct Subscription {
    session: Session,
    app_id: String,
    tokens: [i64; 3],
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.session.RemovePlaybackInfoChanged(self.tokens[0]);
        let _ = self.session.RemoveMediaPropertiesChanged(self.tokens[1]);
        let _ = self.session.RemoveTimelinePropertiesChanged(self.tokens[2]);
    }
}

struct Worker<F: Fn(Update)> {
    tx: Sender<Msg>,
    manager: Manager,
    sub: Option<Subscription>,
    last: MediaState,
    last_art_key: Option<String>,
    on_update: F,
}

impl<F: Fn(Update)> Worker<F> {
    fn new(tx: Sender<Msg>, on_update: F) -> Result<Self> {
        let manager = Manager::RequestAsync()?.join()?;
        let t = tx.clone();
        manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Msg::Refresh);
            Ok(())
        }))?;
        let t = tx.clone();
        manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Msg::Refresh);
            Ok(())
        }))?;
        Ok(Self { tx, manager, sub: None, last: MediaState::default(), last_art_key: None, on_update })
    }

    /// Prefers a session that is actually playing, otherwise the system's current one.
    fn pick_session(&self) -> Option<Session> {
        if let Ok(sessions) = self.manager.GetSessions() {
            for s in sessions {
                let playing = s
                    .GetPlaybackInfo()
                    .and_then(|p| p.PlaybackStatus())
                    .map(|st| st == Status::Playing)
                    .unwrap_or(false);
                if playing {
                    return Some(s);
                }
            }
        }
        self.manager.GetCurrentSession().ok()
    }

    fn subscribe(&mut self, session: &Session) -> Result<()> {
        let app_id = session.SourceAppUserModelId()?.to_string();
        if self.sub.as_ref().is_some_and(|s| s.app_id == app_id) {
            return Ok(());
        }
        self.sub = None;
        macro_rules! refresh_on {
            () => {{
                let t = self.tx.clone();
                &TypedEventHandler::new(move |_, _| {
                    let _ = t.send(Msg::Refresh);
                    Ok(())
                })
            }};
        }
        let tokens = [
            session.PlaybackInfoChanged(refresh_on!())?,
            session.MediaPropertiesChanged(refresh_on!())?,
            session.TimelinePropertiesChanged(refresh_on!())?,
        ];
        self.sub = Some(Subscription { session: session.clone(), app_id, tokens });
        Ok(())
    }

    fn refresh(&mut self) {
        let Some(session) = self.pick_session() else {
            self.sub = None;
            self.publish(MediaState::default());
            if self.last_art_key.take().is_some() {
                (self.on_update)(Update::Art(None));
            }
            return;
        };
        if let Err(e) = self.subscribe(&session) {
            eprintln!("subscribe: {e}");
        }
        match read_state(&session) {
            Ok(state) => {
                let art_key = format!("{}|{}", state.app_id, state.track_key());
                let track_changed = self.last_art_key.as_deref() != Some(&art_key);
                self.publish(state);
                // Players often publish the thumbnail slightly after the title,
                // so re-read it whenever properties change until we have one.
                if track_changed || self.last_art_key.is_none() {
                    let art = read_art(&session).ok().flatten();
                    let got = art.is_some();
                    (self.on_update)(Update::Art(art));
                    self.last_art_key = got.then_some(art_key);
                }
            }
            Err(e) => eprintln!("read state: {e}"),
        }
    }

    fn publish(&mut self, state: MediaState) {
        if state != self.last {
            self.last = state.clone();
            (self.on_update)(Update::State(state, Instant::now()));
        }
    }

    fn command(&mut self, cmd: Command) {
        let Some(sub) = &self.sub else { return };
        let s = &sub.session;
        let r = match cmd {
            Command::PlayPause => s.TryTogglePlayPauseAsync().and_then(|a| a.join()),
            Command::Next => s.TrySkipNextAsync().and_then(|a| a.join()),
            Command::Prev => s.TrySkipPreviousAsync().and_then(|a| a.join()),
            // GSMTC positions are in 100 ns ticks.
            Command::Seek(ms) => s.TryChangePlaybackPositionAsync(ms * 10_000).and_then(|a| a.join()),
        };
        if let Err(e) = r {
            eprintln!("command {cmd:?}: {e}");
        }
    }
}

fn now_filetime_ticks() -> i64 {
    let ft = unsafe { GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

fn read_state(session: &Session) -> Result<MediaState> {
    let app_id = session.SourceAppUserModelId()?.to_string();
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let info = session.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let timeline = session.GetTimelineProperties()?;

    let playing = info.PlaybackStatus()? == Status::Playing;
    let start = timeline.StartTime()?.Duration;
    let end = timeline.EndTime()?.Duration;
    let duration_ms = ((end - start) / 10_000).max(0);
    let mut position_ms = (timeline.Position()?.Duration - start) / 10_000;

    // The timeline is a snapshot taken at LastUpdatedTime; bring it to "now".
    if playing {
        let updated = timeline.LastUpdatedTime()?.UniversalTime;
        let elapsed_ms = (now_filetime_ticks() - updated) / 10_000;
        if updated > 0 && (0..duration_ms.max(1)).contains(&elapsed_ms) {
            position_ms += elapsed_ms;
        }
    }
    if duration_ms > 0 {
        position_ms = position_ms.clamp(0, duration_ms);
    }

    let mut artist = props.Artist()?.to_string();
    if artist.is_empty() {
        artist = props.AlbumArtist()?.to_string();
    }

    Ok(MediaState {
        present: true,
        app_id,
        title: props.Title()?.to_string(),
        artist,
        album: props.AlbumTitle()?.to_string(),
        playing,
        position_ms,
        duration_ms,
        can_seek: controls.IsPlaybackPositionEnabled().unwrap_or(false) && duration_ms > 0,
        can_next: controls.IsNextEnabled().unwrap_or(true),
        can_prev: controls.IsPreviousEnabled().unwrap_or(true),
    })
}

fn read_art(session: &Session) -> Result<Option<Art>> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let Ok(thumb) = props.Thumbnail() else { return Ok(None) };
    let stream = thumb.OpenReadAsync()?.join()?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Ok(None);
    }
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    reader.LoadAsync(size)?.join()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(decode_art(&bytes))
}

/// Decodes the thumbnail, crops it square (Spotify pads with bars in some
/// versions), shrinks it to 160 px and extracts an accent colour.
fn decode_art(bytes: &[u8]) -> Option<Art> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let img = img.resize_exact(160, 160, image::imageops::FilterType::Triangle).to_rgba8();
    let accent = accent_color(&img);
    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height());
    Some(Art { pixels, accent })
}

/// Saturation-weighted average colour, lifted so it stays readable on black.
fn accent_color(img: &image::RgbaImage) -> [u8; 3] {
    let (mut r, mut g, mut b, mut wsum) = (0f32, 0f32, 0f32, 0f32);
    for p in img.pixels().step_by(3) {
        let [pr, pg, pb, _] = p.0.map(|c| c as f32 / 255.0);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        let w = sat * sat * max + 0.002;
        r += pr * w;
        g += pg * w;
        b += pb * w;
        wsum += w;
    }
    let (r, g, b) = (r / wsum, g / wsum, b / wsum);
    let max = r.max(g).max(b).max(0.001);
    // Normalise brightness to ~0.9 and keep some saturation.
    let k = 0.92 / max;
    let mix = |c: f32| ((c * k) * 0.85 + 0.15).clamp(0.0, 1.0);
    [(mix(r) * 255.0) as u8, (mix(g) * 255.0) as u8, (mix(b) * 255.0) as u8]
}
