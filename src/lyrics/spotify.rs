//! Spotify lyrics through the (unofficial) web-player endpoints.
//!
//! Flow: `sp_dc` cookie → web-player access token (TOTP-protected) →
//! search the track id → `color-lyrics` endpoint (Musixmatch-backed, synced).
//! This is not a public API and may break whenever Spotify changes it; the
//! other providers act as a fallback.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha1::Sha1;

use super::{clean_title, main_artist, normalize, Line, Lyrics, Query, Result};
use crate::config::LyricsConfig;

/// Fallback secret (version 5) used when the secrets URL can't be reached.
const FALLBACK_SECRET: (u32, &[u8]) = (5, &[12, 56, 76, 33, 88, 44, 88, 33, 78, 78, 11, 66, 22, 22, 55, 69, 54]);

struct Token {
    value: String,
    expires_ms: u64,
    sp_dc: String,
}

static TOKEN: Mutex<Option<Token>> = Mutex::new(None);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Spotify obfuscates the TOTP secret: XOR each byte, then use the decimal
/// digits of the result as the HMAC key.
fn secret_key(cipher: &[u8]) -> Vec<u8> {
    cipher
        .iter()
        .enumerate()
        .map(|(i, b)| (b ^ ((i % 33) as u8 + 9)).to_string())
        .collect::<String>()
        .into_bytes()
}

fn totp(key: &[u8], unix_secs: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("any key length");
    mac.update(&(unix_secs / 30).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let code = (u32::from(h[o] & 0x7f) << 24)
        | (u32::from(h[o + 1]) << 16)
        | (u32::from(h[o + 2]) << 8)
        | u32::from(h[o + 3]);
    format!("{:06}", code % 1_000_000)
}

/// Latest (version, cipher) from the community-maintained secrets file.
fn latest_secret(agent: &ureq::Agent, url: &str) -> (u32, Vec<u8>) {
    let fetched = (!url.is_empty())
        .then(|| agent.get(url).call().ok()?.into_json::<Value>().ok())
        .flatten()
        .and_then(|v| {
            v.as_object()?
                .iter()
                .filter_map(|(k, v)| {
                    let ver: u32 = k.parse().ok()?;
                    let bytes: Vec<u8> = v.as_array()?.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
                    Some((ver, bytes))
                })
                .max_by_key(|(ver, _)| *ver)
        });
    fetched.unwrap_or_else(|| (FALLBACK_SECRET.0, FALLBACK_SECRET.1.to_vec()))
}

fn access_token(agent: &ureq::Agent, cfg: &LyricsConfig) -> Result<String> {
    let sp_dc = cfg.spotify_sp_dc.trim();
    {
        let guard = TOKEN.lock().unwrap();
        if let Some(t) = guard.as_ref() {
            if t.sp_dc == sp_dc && t.expires_ms > now_ms() + 60_000 {
                return Ok(t.value.clone());
            }
        }
    }

    let cookie = format!("sp_dc={sp_dc}");
    let server_secs = agent
        .get("https://open.spotify.com/api/server-time")
        .call()
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .and_then(|v| v["serverTime"].as_u64())
        .unwrap_or(now_ms() / 1000);

    let (ver, cipher) = latest_secret(agent, &cfg.spotify_secrets_url);
    let key = secret_key(&cipher);
    let otp = totp(&key, now_ms() / 1000);
    let otp_server = totp(&key, server_secs);

    let resp: Value = agent
        .get("https://open.spotify.com/api/token")
        .set("Cookie", &cookie)
        .set("App-Platform", "WebPlayer")
        .query("reason", "init")
        .query("productType", "web-player")
        .query("totp", &otp)
        .query("totpServer", &otp_server)
        .query("totpVer", &ver.to_string())
        .call()?
        .into_json()?;

    if resp["isAnonymous"].as_bool() == Some(true) {
        return Err("Spotify: cookie sp_dc недействителен или устарел".into());
    }
    let value = resp["accessToken"].as_str().ok_or("Spotify: no accessToken")?.to_string();
    let expires_ms = resp["accessTokenExpirationTimestampMs"].as_u64().unwrap_or(now_ms() + 30 * 60_000);
    *TOKEN.lock().unwrap() = Some(Token { value: value.clone(), expires_ms, sp_dc: sp_dc.to_string() });
    Ok(value)
}

fn find_track_id(agent: &ureq::Agent, token: &str, q: &Query) -> Result<Option<String>> {
    let title = clean_title(&q.title);
    let artist = main_artist(&q.artist);
    let queries = [format!("track:{title} artist:{artist}"), format!("{artist} {title}")];
    let want = normalize(&title);

    for query in queries {
        let v: Value = agent
            .get("https://api.spotify.com/v1/search")
            .set("Authorization", &format!("Bearer {token}"))
            .query("q", &query)
            .query("type", "track")
            .query("limit", "10")
            .call()?
            .into_json()?;
        let items = v["tracks"]["items"].as_array().cloned().unwrap_or_default();
        let best = items
            .iter()
            .filter(|t| {
                let name = normalize(t["name"].as_str().unwrap_or_default());
                name.contains(&want) || want.contains(&name)
            })
            .min_by_key(|t| (t["duration_ms"].as_i64().unwrap_or(0) - q.duration_ms).abs());
        if let Some(id) = best.and_then(|t| t["id"].as_str()) {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

pub fn fetch(q: &Query, cfg: &LyricsConfig) -> Result<Option<Lyrics>> {
    let agent = super::agent();
    let token = access_token(&agent, cfg)?;
    let Some(id) = find_track_id(&agent, &token, q)? else { return Ok(None) };

    let resp = agent
        .get(&format!("https://spclient.wg.spotify.com/color-lyrics/v2/track/{id}"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("App-Platform", "WebPlayer")
        .query("format", "json")
        .query("vocalRemoval", "false")
        .query("market", "from_token")
        .call();
    let v: Value = match resp {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let lyr = &v["lyrics"];
    let synced = lyr["syncType"].as_str() == Some("LINE_SYNCED");
    let lines: Vec<Line> = lyr["lines"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|l| Line {
                    time: if synced {
                        l["startTimeMs"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
                    } else {
                        -1
                    },
                    text: l["words"].as_str().unwrap_or_default().replace('♪', "").trim().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(Lyrics { lines, synced, source: "Spotify".into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vector() {
        // RFC 6238 SHA-1 test key, T = 59 → 94287082 (last 6 digits).
        assert_eq!(totp(b"12345678901234567890", 59), "287082");
    }
}
