//! On-disk lyrics cache. Misses are cached too (for a few days) so we don't
//! hammer the APIs for songs that have no lyrics.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::{Lyrics, Query};

const MISS_TTL: Duration = Duration::from_secs(3 * 24 * 3600);

#[derive(Serialize, Deserialize)]
struct Entry {
    lyrics: Option<Lyrics>,
    /// Providers that were asked when this entry was a miss.
    #[serde(default)]
    tried: Vec<String>,
}

fn dir() -> PathBuf {
    crate::config::cache_dir().join("lyrics")
}

pub fn key(q: &Query) -> String {
    // DefaultHasher is not stable across Rust versions; FNV-1a is.
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 = (self.0 ^ *b as u64).wrapping_mul(0x100000001b3);
            }
        }
    }
    let mut h = Fnv(0xcbf29ce484222325);
    super::normalize(&q.artist).hash(&mut h);
    super::normalize(&q.title).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// `Some(None)` = cached miss, `None` = not in cache. A miss is ignored when a
/// provider that was not tried back then is available now.
pub fn load(key: &str, providers: &[String]) -> Option<Option<Lyrics>> {
    let path = dir().join(format!("{key}.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    if entry.lyrics.is_none() {
        if providers.iter().any(|p| !entry.tried.contains(p)) {
            return None;
        }
        let age = std::fs::metadata(&path).ok()?.modified().ok()?;
        if SystemTime::now().duration_since(age).unwrap_or_default() > MISS_TTL {
            return None;
        }
    }
    Some(entry.lyrics)
}

pub fn store(key: &str, lyrics: Option<&Lyrics>, tried: &[String]) {
    let _ = std::fs::create_dir_all(dir());
    let entry = Entry { lyrics: lyrics.cloned(), tried: tried.to_vec() };
    if let Ok(json) = serde_json::to_string(&entry) {
        let _ = std::fs::write(dir().join(format!("{key}.json")), json);
    }
}

pub fn clear() {
    let _ = std::fs::remove_dir_all(dir());
}
