//! User configuration stored in `%APPDATA%\DynamicIsland\config.toml`.

use serde::Deserialize;
use std::path::PathBuf;

const SPOTIFY_SECRETS_URL: &str =
    "https://raw.githubusercontent.com/Thereallo1026/spotify-secrets/refs/heads/main/secrets/secretDict.json";

const DEFAULT_CONFIG: &str = r#"# Dynamic Island — настройки
# После изменения нажмите «Применить настройки» в меню значка в трее.

[ui]
# Прятать остров, когда ничего не играет
hide_when_idle = true
# Раскрывать остров при наведении курсора (иначе — по клику)
expand_on_hover = true
# Показывать текущую строку текста песни в свёрнутом острове
compact_lyrics = true
# Анимированный эквалайзер в свёрнутом острове (≈ +1% GPU во время воспроизведения)
visualizer = true
# Отступ от верхнего края экрана в пикселях
top_offset = 8
# Прятать остров, когда открыта полноэкранная игра или видео
hide_in_fullscreen = true
# Сжимать остров в тонкую полоску, когда под ним окно (например, развёрнутый
# браузер со вкладками). Наведите на полоску — откроется плеер.
auto_tuck = true

[lyrics]
# Порядок источников текста: "auto" или список из "spotify", "yandex", "lrclib".
# "auto" — сначала источник того плеера, который сейчас играет, потом остальные.
providers = ["auto"]

# Сдвиг синхронизации текста в миллисекундах (+ — строки раньше, − — позже)
offset_ms = 150

# Spotify: значение cookie `sp_dc` с сайта open.spotify.com (см. README).
# Без него тексты Spotify недоступны, будут использоваться другие источники.
spotify_sp_dc = ""

# Откуда брать актуальные TOTP-секреты веб-плеера Spotify.
spotify_secrets_url = "https://raw.githubusercontent.com/Thereallo1026/spotify-secrets/refs/heads/main/secrets/secretDict.json"

# Яндекс Музыка: OAuth-токен (см. README).
yandex_token = ""
"#;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub lyrics: LyricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub hide_when_idle: bool,
    pub expand_on_hover: bool,
    pub compact_lyrics: bool,
    pub visualizer: bool,
    pub top_offset: i32,
    pub hide_in_fullscreen: bool,
    pub auto_tuck: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LyricsConfig {
    pub providers: Vec<String>,
    pub offset_ms: i64,
    pub spotify_sp_dc: String,
    pub spotify_secrets_url: String,
    pub yandex_token: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            hide_when_idle: true,
            expand_on_hover: true,
            compact_lyrics: true,
            visualizer: true,
            top_offset: 8,
            hide_in_fullscreen: true,
            auto_tuck: true,
        }
    }
}

impl Default for LyricsConfig {
    fn default() -> Self {
        Self {
            providers: vec!["auto".into()],
            offset_ms: 150,
            spotify_sp_dc: String::new(),
            spotify_secrets_url: SPOTIFY_SECRETS_URL.into(),
            yandex_token: String::new(),
        }
    }
}

pub fn app_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("DynamicIsland")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("DynamicIsland")
}

impl Config {
    /// Loads the config, creating a commented default file on first run.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("config.toml: {e}; using defaults");
                Config::default()
            }),
            Err(_) => {
                let _ = std::fs::create_dir_all(app_dir());
                let _ = std::fs::write(&path, DEFAULT_CONFIG);
                Config::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(c.ui.hide_when_idle);
        assert_eq!(c.lyrics.spotify_secrets_url, SPOTIFY_SECRETS_URL);
    }
}
