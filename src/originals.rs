//! Resolve an audio-only substitution for an exact catalog track on the owner's account.

use crate::api::{Api, Res, Track};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
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
    catalog: LookupCache<Vec<String>>,
    verified: LookupCache<f32>,
    russian: LookupCache<bool>,
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

pub fn verified(api: &Api, original: &crate::verification::Recording, candidate: &crate::verification::Recording, lyrics: &crate::api::Lyrics) -> Option<f32> {
    if lyrics.synced.is_empty() && lyrics.text.is_empty() { return None; }
    // A positive proof is specific to both decoded recordings, their order and the lyrics used.
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(original.digest);
    hash.update(candidate.digest);
    for line in &lyrics.synced {
        hash.update(line.from.to_bits().to_le_bytes());
        hash.update(line.to.map(f32::to_bits).unwrap_or(u32::MAX).to_le_bytes());
        hash.update((line.text.len() as u64).to_le_bytes());
        hash.update(line.text.as_bytes());
    }
    if lyrics.synced.is_empty() {
        for line in &lyrics.text {
            hash.update((line.len() as u64).to_le_bytes());
            hash.update(line.as_bytes());
        }
    }
    let key = format!("{:x}", hash.finalize());
    api.originals.verified.find(&key, || {
        // One record per bucket bounds the disk cache even for a long-lived installation.
        let dir = crate::config::cache_dir().join("restored-speech-v1");
        let path = dir.join(&key[..2]);
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok((saved, time)) = serde_json::from_slice::<(String, f32)>(&bytes) {
                if saved == key && time.is_finite() && time >= 0.0 {
                    return Ok(Some(time));
                }
            }
        }
        let time = crate::verification::compare(original, candidate, lyrics);
        if let Some(time) = time {
            let temp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::create_dir_all(&dir).is_ok() &&
                std::fs::write(&temp, serde_json::to_vec(&(&key, time)).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&temp, &path);
                let _ = std::fs::remove_file(&temp);
            }
        }
        Ok(time)
    })
}

fn can_match(track: &Track) -> bool {
    track.duration_ms > 0 && !track.id.is_empty() && track.id.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn should_uncensor(
    api: &Api, track: &Track, text_language: impl FnOnce() -> Res<Option<bool>>,
    vocals: impl FnOnce() -> Option<bool>,
) -> bool {
    if !can_match(track) { return false; }
    let title = normalized(&track.title);
    if crate::language::russian_title(title_variant(&title).0) { return true; }
    api.originals.russian.find(&track.id, || {
        if track.lyrics || title.chars().any(|c| matches!(c, 'а'..='я' | 'ё')) {
            match text_language() {
                Ok(Some(russian)) => return Ok(Some(russian)),
                Err(e) => return if vocals() == Some(true) { Ok(Some(true)) } else { Err(e) },
                Ok(None) => {}
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

use crate::resolver_login;
#[path = "confirmed_audio.rs"]
mod confirmed_audio;
pub(crate) use confirmed_audio::digest as media_digest;

#[derive(Deserialize)]
pub struct OnlineCandidate {
    source: String,
    ticket: String,
    sha256: Option<String>,
    pub next: Option<String>,
}

pub fn online_candidates(track: &Track) -> Res<Vec<OnlineCandidate>> {
    use std::io::Read;
    let Some(token) = resolver_login::credential() else { return Ok(vec![]) };
    let lower = normalized(&track.title);
    let (base, _) = title_variant(&lower);
    let version = normalized(&track.version);
    let title = if version.is_empty() || variant(&version).is_some() { base.to_owned() } else { format!("{base} ({version})") };
    let artists: Vec<_> = track.artist_list.iter().map(|a| &a.name).collect();
    let response = resolver_login::signed("POST", "/search", &token, serde_json::to_vec(&serde_json::json!({"title":title,"artists":artists,"duration_ms":track.duration_ms})).map_err(|e|e.to_string())?)?
        .timeout(Duration::from_secs(24))
        .send().and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    let candidates: Vec<OnlineCandidate> = serde_json::from_reader(response.take(16 * 1024)).map_err(|e| e.to_string())?;
    Ok(candidates.into_iter().filter(|c| c.ticket.len() == 32 && c.ticket.bytes().all(|b| b.is_ascii_hexdigit())
        && matches!(c.source.as_str(), "Bandcamp" | "SoundCloud" | "YouTube Music" | "Audius")).take(12).collect())
}

pub fn online_next(next: &str) -> Res<Vec<OnlineCandidate>> {
    use std::io::Read;
    if next.len()!=32 || !next.bytes().all(|b|b.is_ascii_hexdigit()) { return Ok(vec![]); }
    let token = resolver_login::credential().ok_or("Внешние источники отключены")?;
    let body = serde_json::to_vec(&serde_json::json!({"next":next})).map_err(|e|e.to_string())?;
    let response = resolver_login::signed("POST", "/next", &token, body)?.timeout(Duration::from_secs(24))
        .send().and_then(|r|r.error_for_status()).map_err(|e|e.to_string())?;
    let candidates: Vec<OnlineCandidate> = serde_json::from_reader(response.take(16*1024)).map_err(|e|e.to_string())?;
    Ok(candidates.into_iter().filter(|c|c.ticket.len()==32 && c.ticket.bytes().all(|b|b.is_ascii_hexdigit())
        && matches!(c.source.as_str(),"Bandcamp"|"SoundCloud"|"YouTube Music"|"Audius")).take(12).collect())
}

pub struct ExternalAudio {
    pub buf: std::sync::Arc<crate::stream::Buf>,
    pub format: String,
    pub cached: bool,
    token: String,
}

pub fn online_audio(candidate: &OnlineCandidate, track: &str, original: Option<&str>) -> Res<ExternalAudio> {
    let token = resolver_login::credential().ok_or("Сначала подключите внешние источники")?;
    let mut request = resolver_login::signed("GET", &format!("/audio/{}",candidate.ticket), &token, vec![])?;
    let cached = original.zip(candidate.sha256.as_deref()).and_then(|(original, expected)| {
        if expected.len()!=64 || !expected.bytes().all(|c| c.is_ascii_hexdigit()) { return None; }
        confirmed_audio::read(&confirmed_audio::path(&token,track,original,expected),expected)
    });
    if cached.is_some() {
        request = request.header("If-None-Match",format!("\"{}\"",candidate.sha256.as_deref().unwrap_or_default()));
    }
    let response = request.timeout(Duration::from_secs(48)).send().map_err(|e| e.to_string())?;
    let reused = response.status() == reqwest::StatusCode::NOT_MODIFIED;
    let buf = if reused {
        cached.ok_or("Нет подтверждённой копии")?
    } else {
        let response = response.error_for_status().map_err(|e| e.to_string())?;
        crate::stream::Buf::from_media(response).map_err(|e| e.to_string())?
    };
    Ok(ExternalAudio { buf, format: format!("MP3 · {}",candidate.source), cached: reused,token })
}

pub fn remember_external(result: ExternalAudio, candidate: &OnlineCandidate, track: &str, original: Option<&str>) {
    let Ok(digest) = confirmed_audio::digest(result.buf.reader()) else { return };
    if let Some(original) = original {
        let _ = confirmed_audio::store(&confirmed_audio::path(&result.token,track,original,&digest), &result.buf);
    }
    let body = serde_json::json!({"ticket":candidate.ticket,"sha256":digest}).to_string().into_bytes();
    std::thread::spawn(move || {
        if let Ok(request) = resolver_login::signed("POST","/confirm",&result.token,body) {
            let _ = request.send();
        }
    });
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
        assert!(should_uncensor(&api, &make(1, "Счастье"), || Ok(None), || panic!("clear title")));
        assert!(should_uncensor(&api, &make(2, "Cadillac"), || Ok(None), || Some(true)));
        let foreign = make(3, "Without Me");
        assert!(!should_uncensor(&api, &foreign, || Ok(None), || Some(false)));
        assert!(!should_uncensor(&api, &foreign, || Ok(None), || panic!("cached foreign language")));
        let unknown = make(4, "Instrumental");
        assert!(!should_uncensor(&api, &unknown, || Ok(None), || None));
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
