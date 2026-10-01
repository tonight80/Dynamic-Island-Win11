//! Yandex Music lyrics through the (unofficial) mobile API — the same one the
//! Android app uses. Requires the user's OAuth token.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use super::{clean_title, lrc, main_artist, normalize, Lyrics, Query, Result};
use crate::config::LyricsConfig;

const API: &str = "https://api.music.yandex.net";
const SIGN_KEY: &[u8] = b"p93jhgh689SBReK6ghtw62";
const CLIENT: &str = "YandexMusicAndroid/24023621";

fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.split(':').next().unwrap_or(s).to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

struct Found {
    id: String,
    has_sync: bool,
    has_text: bool,
}

fn search(agent: &ureq::Agent, auth: &str, q: &Query) -> Result<Option<Found>> {
    let title = clean_title(&q.title);
    let text = format!("{} {}", main_artist(&q.artist), title);
    let v: Value = agent
        .get(&format!("{API}/search"))
        .set("Authorization", auth)
        .set("X-Yandex-Music-Client", CLIENT)
        .query("text", &text)
        .query("type", "track")
        .query("page", "0")
        .call()?
        .into_json()?;

    let want = normalize(&title);
    let results = v["result"]["tracks"]["results"].as_array().cloned().unwrap_or_default();
    let best = results
        .iter()
        .filter(|t| {
            let name = normalize(t["title"].as_str().unwrap_or_default());
            name.contains(&want) || want.contains(&name)
        })
        .min_by_key(|t| (t["durationMs"].as_i64().unwrap_or(0) - q.duration_ms).abs());
    Ok(best.and_then(|t| {
        Some(Found {
            id: id_of(&t["id"])?,
            has_sync: t["lyricsInfo"]["hasAvailableSyncLyrics"].as_bool().unwrap_or(false),
            has_text: t["lyricsInfo"]["hasAvailableTextLyrics"].as_bool().unwrap_or(false),
        })
    }))
}

fn download(agent: &ureq::Agent, auth: &str, id: &str, format: &str) -> Result<Option<String>> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(SIGN_KEY).expect("any key length");
    mac.update(format!("{id}{ts}").as_bytes());
    let sign = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let resp = agent
        .get(&format!("{API}/tracks/{id}/lyrics"))
        .set("Authorization", auth)
        .set("X-Yandex-Music-Client", CLIENT)
        .query("format", format)
        .query("timeStamp", &ts)
        .query("sign", &sign)
        .call();
    let v: Value = match resp {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(url) = v["result"]["downloadUrl"].as_str() else { return Ok(None) };
    Ok(Some(agent.get(url).call()?.into_string()?))
}

pub fn fetch(q: &Query, cfg: &LyricsConfig) -> Result<Option<Lyrics>> {
    let agent = super::agent();
    let auth = format!("OAuth {}", cfg.yandex_token.trim());
    let Some(found) = search(&agent, &auth, q)? else { return Ok(None) };

    if found.has_sync {
        if let Some(text) = download(&agent, &auth, &found.id, "LRC")? {
            let lines = lrc::parse(&text);
            if !lines.is_empty() {
                return Ok(Some(Lyrics { lines, synced: true, source: "Яндекс Музыка".into() }));
            }
        }
    }
    if found.has_text {
        if let Some(text) = download(&agent, &auth, &found.id, "TEXT")? {
            if !text.trim().is_empty() {
                return Ok(Some(Lyrics { lines: lrc::plain(&text), synced: false, source: "Яндекс Музыка".into() }));
            }
        }
    }
    Ok(None)
}
