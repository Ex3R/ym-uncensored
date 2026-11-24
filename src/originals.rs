//! Resolve an audio-only substitution for an exact catalog track on the owner's account.

use crate::api::{Api, Res, Track};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MATCH_TOLERANCE_MS: u64 = 2000;
const CACHE_TTL: Duration = Duration::from_secs(900);

#[derive(Default)]
struct LookupCache<T>(Mutex<HashMap<String, (Instant, Option<T>)>>);

impl<T: Clone> LookupCache<T> {
    fn find(&self, track_id: &str, search: impl FnOnce() -> Res<Option<T>>) -> Option<T> {
        if let Some((_, result)) = self.0.lock().unwrap().get(track_id)
            .filter(|(time, _)| time.elapsed() < CACHE_TTL)
        {
            return result.clone();
        }
        let result = search().ok()?;
        let mut entries = self.0.lock().unwrap();
        if entries.len() >= 512 {
            entries.retain(|_, (time, _)| time.elapsed() < CACHE_TTL);
            if entries.len() >= 512 {
                entries.clear();
            }
        }
        entries.insert(track_id.into(), (Instant::now(), result.clone()));
        result
    }
}

#[derive(Default)]
pub(crate) struct Originals {
    playlist: OnceLock<PlaylistUploads>,
    catalog: LookupCache<Vec<String>>,
    verified: LookupCache<f32>,
    russian: LookupCache<bool>,
}

#[derive(Deserialize)]
struct Entry {
    upload_id: String,
    duration_ms: u64,
}

#[derive(Deserialize)]
struct Library {
    account_uid: String,
    tracks: HashMap<String, Entry>,
}

#[derive(Default)]
struct PlaylistUploads {
    by_recording: HashMap<(String, Vec<String>), Vec<Entry>>,
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn recording(title: &str, artists: impl Iterator<Item = String>) -> Option<(String, Vec<String>)> {
    let title = normalized(title);
    let mut artists: Vec<_> = artists.map(|artist| normalized(&artist)).filter(|name| !name.is_empty()).collect();
    artists.sort_unstable();
    artists.dedup();
    (!title.is_empty() && !artists.is_empty()).then_some((title, artists))
}

impl PlaylistUploads {
    fn from_entries(entries: Vec<Value>) -> Self {
        let mut library = Self::default();
        for entry in entries {
            let track = &entry["track"];
            let Some(id) = track["id"].as_str().filter(|id| valid_upload_id(id)) else { continue };
            if track["trackSource"].as_str() != Some("UGC") || track["state"].as_str() != Some("playable")
                || track["available"].as_bool() != Some(true)
            {
                continue;
            }
            let Some(title) = track["title"].as_str() else { continue };
            let Some(artists) = track["ugcArtistName"].as_str() else { continue };
            let Some(duration_ms) = track["durationMs"].as_u64().filter(|duration| *duration > 0) else { continue };
            if let Some(key) = recording(title, artists.split(',').map(String::from)) {
                library.by_recording.entry(key).or_default().push(Entry { upload_id: id.into(), duration_ms });
            }
        }
        library
    }

    fn candidates(&self, track: &Track) -> Vec<&str> {
        let title = if track.version.is_empty() { track.title.clone() } else { format!("{} ({})", track.title, track.version) };
        let key = recording(&title, track.artist_list.iter().map(|artist| artist.name.clone()));
        key.and_then(|key| self.by_recording.get(&key)).into_iter().flatten()
            .filter(|entry| entry.duration_ms.abs_diff(track.duration_ms) <= MATCH_TOLERANCE_MS)
            .map(|entry| entry.upload_id.as_str()).collect()
    }
}

impl Library {
    fn candidate(&self, catalog_id: &str, duration_ms: u64) -> Option<&str> {
        let entry = self.tracks.get(catalog_id)?;
        (duration_ms > 0
            && entry.duration_ms > 0
            && entry.duration_ms.abs_diff(duration_ms) <= MATCH_TOLERANCE_MS
            && valid_upload_id(&entry.upload_id))
            .then_some(entry.upload_id.as_str())
    }
}

fn valid_upload_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

fn bundle_library() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    Some(executable.parent()?.parent()?.join("Resources").join("originals.json"))
}

fn read_library() -> Option<Library> {
    let config_path = crate::config::dir().join("originals.json");
    let path = std::env::var_os("ANCENSOR_LIBRARY").map(PathBuf::from).or_else(|| {
        if config_path.is_file() { Some(config_path) } else { bundle_library() }
    })?;
    match std::fs::read(&path) {
        Ok(contents) => match serde_json::from_slice(&contents) {
            Ok(library) => Some(library),
            Err(error) => {
                crate::config::log(format!("Не удалось прочитать таблицу личных версий: {error}"));
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            crate::config::log(format!("Не удалось открыть таблицу личных версий: {error}"));
            None
        }
    }
}

pub fn prepare(api: &Api) {
    api.originals.playlist.get_or_init(|| match api.original_playlist_entries() {
        Ok(entries) => PlaylistUploads::from_entries(entries),
        Err(error) => {
            crate::config::log(format!("Не удалось прочитать личный плейлист: {error}"));
            PlaylistUploads::default()
        }
    });
}

pub fn mapped_upload(api: &Api, track: &Track) -> Option<&'static str> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    if let Some(library) = LIBRARY.get_or_init(read_library).as_ref() {
        if let Some(id) = library.candidate(&track.id, track.duration_ms) {
            if api.account_uid().ok().as_deref() == Some(library.account_uid.as_str()) {
                return Some(id);
            }
        }
    }
    None
}

pub fn playlist_candidates<'a>(api: &'a Api, track: &Track) -> Vec<&'a str> {
    prepare(api);
    api.originals.playlist.get().map_or_else(Vec::new, |p| p.candidates(track))
}

pub fn verified(api: &Api, original: &crate::verification::Recording, candidate: &crate::verification::Recording, lyrics: &crate::api::Lyrics) -> Option<f32> {
    if lyrics.synced.is_empty() && lyrics.text.is_empty() { return None; }
    let key = format!("{:02x?}/{:02x?}", original.digest, candidate.digest);
    api.originals.verified.find(&key, || Ok(crate::verification::compare(original, candidate, lyrics)))
}

fn can_match(track: &Track) -> bool {
    track.duration_ms > 0 && !track.id.is_empty() && track.id.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn should_uncensor(api: &Api, track: &Track, vocals: impl FnOnce() -> Option<bool>) -> bool {
    if !can_match(track) { return false; }
    let title = normalized(&track.title);
    if crate::language::russian_title(title_variant(&title).0) { return true; }
    api.originals.russian.find(&track.id, || {
        if track.lyrics {
            let deadline = Instant::now() + Duration::from_millis(1500);
            if let Ok(lyrics) = api.lyrics_until(&track.id, Some(deadline)) {
                if let Some(russian) = crate::language::lyrics_language(&lyrics) {
                    return Ok(Some(russian));
                }
            }
        }
        if title.chars().any(|c| matches!(c, 'а'..='я' | 'ё')) {
            if let Some(russian) = api.reference_lyrics(track).as_ref().and_then(crate::language::lyrics_language) {
                return Ok(Some(russian));
            }
        }
        vocals().map(Some).ok_or_else(|| "язык не определён".into())
    }).unwrap_or(false)
}

fn variant(text: &str) -> Option<bool> {
    match text {
        "explicit" | "explicit version" | "uncensored" | "uncut" | "без цензуры" => Some(true),
        "clean" | "clean version" | "цензура" | "с цензурой" | "без мата" => Some(false),
        _ => None,
    }
}

fn title_variant(title: &str) -> (&str, Option<bool>) {
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some((base, suffix)) = title.strip_suffix(close).and_then(|t| t.rsplit_once(open)) {
            if let Some(explicit) = variant(suffix.trim()) {
                return (base.trim_end(), Some(explicit));
            }
        }
    }
    (title, None)
}

fn catalog_candidates<'a>(track: &Track, candidates: &'a [Track]) -> Vec<&'a str> {
    let title = normalized(&track.title);
    let (title, source_marker) = title_variant(&title);
    let version = normalized(&track.version);
    let source_marker = variant(&version).or(source_marker);
    let stable_version = if variant(&version).is_some() { "" } else { &version };
    let Some(recording_key) = recording(title, track.artist_list.iter().map(|a| a.id.clone())) else { return vec![] };
    let hits = candidates.iter().filter(|candidate| {
        if candidate.id == track.id || !can_match(candidate)
            || track.duration_ms.abs_diff(candidate.duration_ms) > MATCH_TOLERANCE_MS
        {
            return false;
        }
        let candidate_title = normalized(&candidate.title);
        let (candidate_title, marker) = title_variant(&candidate_title);
        let candidate_version = normalized(&candidate.version);
        let marker = variant(&candidate_version).or(marker);
        let explicit = marker == Some(true)
            || (candidate.explicit && marker != Some(false) && (!track.explicit || source_marker == Some(false)));
        let candidate_version = if variant(&candidate_version).is_some() { "" } else { &candidate_version };
        explicit
            && stable_version == candidate_version
            && recording(candidate_title, candidate.artist_list.iter().map(|a| a.id.clone())).as_ref() == Some(&recording_key)
    });
    hits.map(|candidate| candidate.id.as_str()).collect()
}

pub fn catalog_versions(api: &Api, track: &Track) -> Vec<String> {
    let Some(query) = recording_query(track) else { return vec![] };
    api.originals.catalog.find(&track.id, || {
        let candidates = api.search_recording(&query)?;
        Ok(Some(catalog_candidates(track, &candidates).into_iter().map(String::from).collect()))
    }).unwrap_or_default()
}

fn recording_query(track: &Track) -> Option<String> {
    if !can_match(track) || track.title.trim().is_empty() || track.artist_list.is_empty() {
        return None;
    }
    let lower = normalized(&track.title);
    let (title, _) = title_variant(&lower);
    Some(format!("{title} {}", track.artists))
}

const RESOLVER: &str = "https://ym.idkroma.ru/api/uncensored/v1";

#[derive(Deserialize)]
pub struct OnlineCandidate {
    source: String,
    ticket: String,
}

fn resolver_http() -> Res<reqwest::blocking::Client> {
    // Yandex account credentials belong only to the catalog client.
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build().map_err(|e| e.to_string())
}

pub fn online_candidates(track: &Track) -> Res<Vec<OnlineCandidate>> {
    use std::io::Read;
    let lower = normalized(&track.title);
    let (base, _) = title_variant(&lower);
    let version = normalized(&track.version);
    let title = if version.is_empty() || variant(&version).is_some() { base.to_owned() } else { format!("{base} ({version})") };
    let artists: Vec<_> = track.artist_list.iter().map(|a| &a.name).collect();
    let response = resolver_http()?.post(format!("{RESOLVER}/search"))
        .timeout(Duration::from_secs(19))
        .json(&serde_json::json!({"title":title, "artists":artists, "duration_ms":track.duration_ms}))
        .send().and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    let candidates: Vec<OnlineCandidate> = serde_json::from_reader(response.take(16 * 1024)).map_err(|e| e.to_string())?;
    Ok(candidates.into_iter().filter(|c| c.ticket.len() == 32 && c.ticket.bytes().all(|b| b.is_ascii_hexdigit())
        && matches!(c.source.as_str(), "Bandcamp" | "SoundCloud" | "YouTube Music")).take(12).collect())
}

pub fn online_audio(candidate: &OnlineCandidate) -> Res<(std::sync::Arc<crate::stream::Buf>, String)> {
    let response = resolver_http()?.get(format!("{RESOLVER}/audio/{}", candidate.ticket))
        .timeout(Duration::from_secs(48)).send().and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    let buf = crate::stream::Buf::from_media(response).map_err(|e| e.to_string())?;
    Ok((buf, format!("MP3 · {}", candidate.source)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_or_confirmed_vocals_enable_search_and_foreign_vocals_stay_excluded() {
        let api = Api::new("");
        let make = |id, title| crate::api::parse_track(&serde_json::json!({
            "id": id, "title": title, "durationMs": 120000, "artists": [{"id": 1, "name": "Русский артист"}]
        })).unwrap();
        assert!(should_uncensor(&api, &make(1, "Счастье"), || panic!("clear title")));
        assert!(should_uncensor(&api, &make(2, "Cadillac"), || Some(true)));
        let foreign = make(3, "Without Me");
        assert!(!should_uncensor(&api, &foreign, || Some(false)));
        assert!(!should_uncensor(&api, &foreign, || panic!("cached foreign language")));
        let unknown = make(4, "Instrumental");
        assert!(!should_uncensor(&api, &unknown, || None));
    }

    #[test]
    fn chooses_only_the_exact_catalog_recording_with_a_compatible_duration() {
        let library: Library = serde_json::from_str(r#"{
            "account_uid": "42",
            "tracks": {
                "123": {"upload_id": "00000000-0000-4000-8000-000000000001", "duration_ms": 120000},
                "124": {"upload_id": "https://example.org/song.mp3", "duration_ms": 120000}
            }
        }"#).unwrap();
        assert_eq!(library.candidate("123", 121000), Some("00000000-0000-4000-8000-000000000001"));
        assert_eq!(library.candidate("123", 125000), None);
        assert_eq!(library.candidate("123", 0), None);
        assert_eq!(library.candidate("124", 120000), None);
        assert_eq!(library.candidate("125", 120000), None);
    }

    #[test]
    fn playlist_only_supplies_matching_candidates_for_audio_verification() {
        let track = crate::api::parse_track(&serde_json::json!({
            "id": 123, "title": "Moonlight", "durationMs": 120000,
            "artists": [{"id": 1, "name": "A"}, {"id": 2, "name": "B"}]
        })).unwrap();
        let row = |id: &str, artists: &str| serde_json::json!({"track": {
            "id": id, "title": " Moonlight ", "ugcArtistName": artists,
            "durationMs": 120500, "trackSource": "UGC", "state": "playable", "available": true,
        }});
        let first = "00000000-0000-4000-8000-000000000001";
        let second = "00000000-0000-4000-8000-000000000002";
        let library = PlaylistUploads::from_entries(vec![row(first, "B, A"), row(second, "C")]);
        assert_eq!(library.candidates(&track), vec![first]);
        let ambiguous = PlaylistUploads::from_entries(vec![row(first, "B, A"), row(second, "A, B")]);
        assert_eq!(ambiguous.candidates(&track), vec![first, second]);
    }

    #[test]
    fn catalog_finds_an_explicit_copy_of_the_same_recording() {
        let make = |id, title, version, duration, artist_id, explicit| crate::api::parse_track(&serde_json::json!({
            "id": id, "title": title, "version": version, "durationMs": duration, "explicit": explicit,
            "artists": [{"id": artist_id, "name": "Artist"}],
        })).unwrap();
        let clean = make(1, "Song (Clean)", "", 120000, 10, true);
        let wrong_artist = make(2, "Song", "", 120000, 20, true);
        let long = make(3, "Song", "", 123000, 10, true);
        let remix = make(4, "Song", "Remix", 120000, 10, true);
        let match_track = make(5, "Song", "", 120100, 10, true);
        let hits = [wrong_artist, long, remix, match_track];
        assert_eq!(catalog_candidates(&clean, &hits), vec!["5"]);
        let unknown = make(6, "Song", "", 120000, 10, true);
        assert!(catalog_candidates(&unknown, &hits).is_empty());
    }
}
