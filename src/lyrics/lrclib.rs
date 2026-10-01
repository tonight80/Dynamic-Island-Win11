//! LRCLIB (https://lrclib.net) — free, open, no key required.

use serde::Deserialize;

use super::{clean_title, lrc, main_artist, normalize, Lyrics, Query, Result};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

const UA: &str = concat!("DynamicIsland/", env!("CARGO_PKG_VERSION"), " (https://github.com/tonight80/Dynamic-Island-Win11)");

pub fn fetch(q: &Query) -> Result<Option<Lyrics>> {
    let agent = super::agent();
    if q.artist.trim().is_empty() {
        return search(&agent, q); // /api/get requires an artist
    }

    // Exact match by signature first.
    let mut req = agent
        .get("https://lrclib.net/api/get")
        .set("User-Agent", UA)
        .query("artist_name", &q.artist)
        .query("track_name", &q.title);
    if !q.album.is_empty() {
        req = req.query("album_name", &q.album);
    }
    if q.duration_ms > 0 {
        req = req.query("duration", &(q.duration_ms / 1000).to_string());
    }
    match req.call() {
        Ok(resp) => {
            let rec: Record = resp.into_json()?;
            if let Some(l) = to_lyrics(rec) {
                return Ok(Some(l));
            }
        }
        Err(ureq::Error::Status(400 | 404, _)) => {}
        Err(e) => return Err(e.into()),
    }
    search(&agent, q)
}

/// Fuzzy search, picking the closest match by title, artist and duration.
fn search(agent: &ureq::Agent, q: &Query) -> Result<Option<Lyrics>> {
    let mut req = agent
        .get("https://lrclib.net/api/search")
        .set("User-Agent", UA)
        .query("track_name", &clean_title(&q.title));
    if !q.artist.trim().is_empty() {
        req = req.query("artist_name", &main_artist(&q.artist));
    }
    let recs: Vec<Record> = req.call()?.into_json()?;

    let want_title = normalize(&clean_title(&q.title));
    let dur = q.duration_ms as f64 / 1000.0;
    let best = recs
        .into_iter()
        .filter(|r| !r.instrumental && (r.synced_lyrics.is_some() || r.plain_lyrics.is_some()))
        .filter(|r| q.duration_ms <= 0 || r.duration <= 0.0 || (r.duration - dur).abs() < 8.0)
        .filter(|r| normalize(&r.track_name).contains(&want_title) || want_title.contains(&normalize(&r.track_name)))
        .max_by_key(|r| {
            let mut score = 0;
            if r.synced_lyrics.is_some() {
                score += 10;
            }
            if normalize(&r.artist_name).contains(&normalize(&main_artist(&q.artist))) {
                score += 5;
            }
            score - (r.duration - dur).abs() as i32
        });
    Ok(best.and_then(to_lyrics))
}

fn to_lyrics(r: Record) -> Option<Lyrics> {
    if let Some(s) = r.synced_lyrics.filter(|s| !s.trim().is_empty()) {
        let lines = lrc::parse(&s);
        if !lines.is_empty() {
            return Some(Lyrics { lines, synced: true, source: "LRCLIB".into() });
        }
    }
    let p = r.plain_lyrics.filter(|s| !s.trim().is_empty())?;
    Some(Lyrics { lines: lrc::plain(&p), synced: false, source: "LRCLIB".into() })
}
