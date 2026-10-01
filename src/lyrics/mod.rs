//! Lyrics lookup: Spotify (unofficial), Yandex Music (unofficial) and LRCLIB,
//! with an on-disk cache so every song is fetched at most once.

mod cache;
mod lrc;
mod lrclib;
mod spotify;
mod yandex;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::LyricsConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line {
    /// Start time in ms; -1 for unsynced lyrics.
    pub time: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub synced: bool,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub app_id: String,
}

pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(BROWSER_UA)
        .build()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Provider {
    Spotify,
    Yandex,
    Lrclib,
}

fn provider_order(cfg: &LyricsConfig, app_id: &str) -> Vec<Provider> {
    let parse = |s: &str| match s.trim().to_ascii_lowercase().as_str() {
        "spotify" => Some(Provider::Spotify),
        "yandex" => Some(Provider::Yandex),
        "lrclib" => Some(Provider::Lrclib),
        _ => None,
    };
    let mut order: Vec<Provider> = Vec::new();
    for p in &cfg.providers {
        if p.eq_ignore_ascii_case("auto") {
            let app = app_id.to_ascii_lowercase();
            let auto = if app.contains("spotify") {
                [Provider::Spotify, Provider::Lrclib, Provider::Yandex]
            } else if app.contains("yandex") || app.contains("яндекс") {
                [Provider::Yandex, Provider::Lrclib, Provider::Spotify]
            } else {
                [Provider::Lrclib, Provider::Yandex, Provider::Spotify]
            };
            order.extend(auto);
        } else if let Some(p) = parse(p) {
            order.push(p);
        }
    }
    if order.is_empty() {
        order.push(Provider::Lrclib);
    }
    let mut seen = Vec::new();
    order.retain(|p| {
        let keep = !seen.contains(p)
            && match p {
                Provider::Spotify => !cfg.spotify_sp_dc.trim().is_empty(),
                Provider::Yandex => !cfg.yandex_token.trim().is_empty(),
                Provider::Lrclib => true,
            };
        seen.push(*p);
        keep
    });
    order
}

/// Blocking lookup; call from a worker thread. `None` means nothing was found.
pub fn fetch(q: &Query, cfg: &LyricsConfig) -> Option<Lyrics> {
    let key = cache::key(q);
    let order = provider_order(cfg, &q.app_id);
    let tried: Vec<String> = order.iter().map(|p| format!("{p:?}")).collect();
    if let Some(hit) = cache::load(&key, &tried) {
        return hit;
    }

    let mut plain: Option<Lyrics> = None;
    let mut had_error = false;
    for provider in order {
        let res = match provider {
            Provider::Spotify => spotify::fetch(q, cfg),
            Provider::Yandex => yandex::fetch(q, cfg),
            Provider::Lrclib => lrclib::fetch(q),
        };
        match res {
            Ok(Some(l)) if l.synced => {
                cache::store(&key, Some(&l), &tried);
                return Some(l);
            }
            Ok(Some(l)) => {
                plain.get_or_insert(l);
            }
            Ok(None) => {}
            Err(e) => {
                had_error = true;
                eprintln!("lyrics {provider:?}: {e}");
            }
        }
    }
    // Don't cache a miss caused by a network error — retry next time.
    if plain.is_some() || !had_error {
        cache::store(&key, plain.as_ref(), &tried);
    }
    plain
}

pub fn clear_cache() {
    cache::clear();
}

/// Strips "(feat. X)", "- Remastered 2011" and similar noise for fuzzy searches.
pub(crate) fn clean_title(title: &str) -> String {
    let mut t = title.to_string();
    for (open, close) in [('(', ')'), ('[', ']')] {
        while let (Some(a), Some(b)) = (t.find(open), t.find(close)) {
            if b <= a {
                break;
            }
            let inner = t[a + 1..b].to_lowercase();
            if ["feat", "ft.", "with", "remaster", "live", "version", "edit", "mix", "prod"]
                .iter()
                .any(|w| inner.contains(w))
            {
                t.replace_range(a..=b, "");
            } else {
                break;
            }
        }
    }
    if let Some(i) = t.find(" - ") {
        let tail = t[i + 3..].to_lowercase();
        if ["remaster", "live", "version", "edit", "mono", "stereo"].iter().any(|w| tail.contains(w)) {
            t.truncate(i);
        }
    }
    t.trim().to_string()
}

/// First artist from "A, B & C" / "A feat. B".
pub(crate) fn main_artist(artist: &str) -> String {
    let lower = artist.to_lowercase();
    let mut cut = artist.len();
    for sep in [",", " & ", " feat", " ft.", " x ", ";"] {
        if let Some(i) = lower.find(sep) {
            cut = cut.min(i);
        }
    }
    artist[..cut].trim().to_string()
}

pub(crate) fn normalize(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}
