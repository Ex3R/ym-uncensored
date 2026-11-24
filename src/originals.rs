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
    !track.user_upload && track.duration_ms > 0 && (1..=20).contains(&track.id.len())
        && !track.id.starts_with('0') && track.id.bytes().all(|byte| byte.is_ascii_digit())
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

impl OnlineCandidate {
    fn valid(&self) -> bool {
        self.ticket.len() == 32 && self.ticket.bytes().all(|b| b.is_ascii_hexdigit())
            && matches!(self.source.as_str(), "Bandcamp" | "SoundCloud" | "YouTube Music" | "Audius")
    }
}

fn digest_valid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn pair_digest(track: &str, baseline: &str, media: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(format!("ym-pair-v1\n{track}\n{baseline}\n{media}").as_bytes()))
}

pub fn public_metadata(track: &Track) -> Option<serde_json::Value> {
    if !can_match(track) || track.title.trim().is_empty() || track.artist_list.is_empty() { return None; }
    let lower = normalized(&track.title);
    let (base, _) = title_variant(&lower);
    let version = normalized(&track.version);
    let title = if version.is_empty() || variant(&version).is_some() { base.to_owned() } else { format!("{base} ({version})") };
    let artists: Vec<_> = track.artist_list.iter().map(|a| &a.name).collect();
    Some(serde_json::json!({"title":title,"artists":artists,"duration_ms":track.duration_ms}))
}

fn request_json<T: serde::de::DeserializeOwned>(path: &str, token: &str, body: serde_json::Value, timeout: Duration) -> Res<T> {
    use std::io::Read;
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let response = resolver_login::signed("POST", path, token, body)?.timeout(timeout)
        .send().and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    serde_json::from_reader(response.take(16 * 1024)).map_err(|e| e.to_string())
}

fn candidates(path: &str, metadata: serde_json::Value) -> Res<Vec<OnlineCandidate>> {
    let Some(token) = resolver_login::credential() else { return Ok(vec![]) };
    let hits: Vec<OnlineCandidate> = request_json(path, &token, metadata, Duration::from_secs(24))?;
    Ok(hits.into_iter().filter(OnlineCandidate::valid).take(12).collect())
}

pub fn online_candidates(track: &Track) -> Res<Vec<OnlineCandidate>> {
    let Some(metadata) = public_metadata(track) else { return Ok(vec![]) };
    candidates("/search", metadata)
}

pub fn online_next(next: &str) -> Res<Vec<OnlineCandidate>> {
    if next.len() != 32 || !next.bytes().all(|b| b.is_ascii_hexdigit()) { return Ok(vec![]); }
    candidates("/next", serde_json::json!({"next":next}))
}

#[derive(Deserialize)]
pub struct KnownCandidate {
    #[serde(flatten)]
    candidate: OnlineCandidate,
    baseline_sha256: Option<String>,
    pair_sha256: Option<String>,
    #[serde(default)]
    baseline_sha256s: Vec<String>,
}

impl KnownCandidate {
    fn cache_key(&self, track: &str) -> Option<String> {
        let media = self.candidate.sha256.as_deref().filter(|s| digest_valid(s))?;
        if self.baseline_sha256s.len() > 16 || self.baseline_sha256s.iter().any(|s| !digest_valid(s) || s == media) { return None; }
        match (&self.baseline_sha256, &self.pair_sha256) {
            (Some(baseline), None) if digest_valid(baseline) && baseline != media => Some(pair_digest(track, baseline, media)),
            (None, Some(pair)) if digest_valid(pair) && self.baseline_sha256s.is_empty() => Some(pair.clone()),
            _ => None,
        }
    }

    pub fn matches(&self, track: &str, baseline: &str) -> bool {
        let Some(media) = self.candidate.sha256.as_deref() else { return false };
        if !digest_valid(baseline) || media == baseline || self.cache_key(track).is_none() { return false; }
        if let Some(pair) = &self.pair_sha256 { return *pair == pair_digest(track, baseline, media); }
        self.baseline_sha256.as_deref() == Some(baseline) || self.baseline_sha256s.iter().any(|s| s == baseline)
    }
}

pub fn known_candidate(track: &str, mut metadata: serde_json::Value, token: &str) -> Option<KnownCandidate> {
    metadata["catalog_id"] = track.into();
    let hits: Vec<KnownCandidate> = request_json("/known", token, metadata, Duration::from_secs(4)).ok()?;
    let [hit]: [KnownCandidate; 1] = hits.try_into().ok()?;
    (hit.candidate.valid() && hit.cache_key(track).is_some()).then_some(hit)
}

pub struct ExternalAudio {
    pub buf: std::sync::Arc<crate::stream::Buf>,
    pub format: String,
    pub cached: bool,
    token: String,
}

fn fetch_audio(candidate: &OnlineCandidate, token: String, cached: Option<std::sync::Arc<crate::stream::Buf>>, timeout: Duration, current: &impl Fn() -> bool) -> Res<ExternalAudio> {
    if !current() { return Err("Выбор трека изменился".into()); }
    let mut request = resolver_login::signed("GET", &format!("/audio/{}", candidate.ticket), &token, vec![])?;
    if cached.is_some() {
        request = request.header("If-None-Match", format!("\"{}\"", candidate.sha256.as_deref().unwrap_or_default()));
    }
    let response = request.timeout(timeout).send().map_err(|e| e.to_string())?;
    let reused = response.status() == reqwest::StatusCode::NOT_MODIFIED;
    let buf = ticket_audio(response, candidate.sha256.as_deref(), cached, current)?;
    Ok(ExternalAudio { buf, format: format!("MP3 · {}", candidate.source), cached: reused, token })
}

fn ticket_audio(response: reqwest::blocking::Response, expected: Option<&str>, cached: Option<std::sync::Arc<crate::stream::Buf>>, current: &impl Fn() -> bool) -> Res<std::sync::Arc<crate::stream::Buf>> {
    match response.status() {
        reqwest::StatusCode::NOT_MODIFIED => cached.ok_or_else(|| "Нет подтверждённой копии".into()),
        reqwest::StatusCode::OK => {
            let buf = crate::stream::Buf::from_media(response, current).map_err(|e| e.to_string())?;
            if let Some(expected) = expected {
                if !digest_valid(expected) || confirmed_audio::digest(buf.reader()).ok().as_deref() != Some(expected) {
                    return Err("Аудио отличается от выданного сервером".into());
                }
            }
            Ok(buf)
        }
        status => Err(format!("Сервер аудио вернул {status}")),
    }
}

fn cached_audio(token: &str, track: &str, media: &str, pair: &str, legacy_baseline: Option<&str>) -> Option<std::sync::Arc<crate::stream::Buf>> {
    let read = |key| confirmed_audio::read(&confirmed_audio::path(token, track, key, media), media);
    read(pair).or_else(|| legacy_baseline.and_then(read))
}

pub fn known_audio(hit: &KnownCandidate, track: &str, token: String, current: &impl Fn() -> bool) -> Option<ExternalAudio> {
    let media = hit.candidate.sha256.as_deref()?;
    let pair = hit.cache_key(track)?;
    let cached = cached_audio(&token, track, media, &pair, hit.baseline_sha256.as_deref()).or_else(|| {
        hit.baseline_sha256s.iter().find_map(|s| cached_audio(&token, track, media, &pair_digest(track, s, media), Some(s)))
    });
    fetch_audio(&hit.candidate, token, cached, Duration::from_secs(15), current).ok()
}

pub fn cache_known(hit: &KnownCandidate, track: &str, audio: &ExternalAudio) {
    if audio.cached { return; }
    if let (Some(pair), Some(media)) = (hit.cache_key(track), &hit.candidate.sha256) {
        let _ = confirmed_audio::store(&confirmed_audio::path(&audio.token, track, &pair, media), &audio.buf);
    }
}

pub fn online_audio(candidate: &OnlineCandidate, track: &str, original: Option<&str>, current: &impl Fn() -> bool) -> Res<ExternalAudio> {
    let token = resolver_login::credential().ok_or("Сначала подключите внешние источники")?;
    let cached = original.zip(candidate.sha256.as_deref()).and_then(|(baseline, media)| {
        if !digest_valid(media) { return None; }
        cached_audio(&token, track, media, &pair_digest(track, baseline, media), Some(baseline))
    });
    fetch_audio(candidate, token, cached, Duration::from_secs(48), current)
}

pub fn remember_external(result: ExternalAudio, candidate: &OnlineCandidate, track: &str, original: Option<&str>) {
    let Ok(digest) = confirmed_audio::digest(result.buf.reader()) else { return };
    let mut body = serde_json::json!({"ticket":candidate.ticket,"sha256":digest});
    if let Some(original) = original {
        let proof = pair_digest(track, original, &digest);
        let path = confirmed_audio::path(&result.token, track, &proof, &digest);
        if !result.cached || !path.exists() { let _ = confirmed_audio::store(&path, &result.buf); }
        body["catalog_id"] = track.into();
        body["pair_sha256"] = proof.into();
    }
    std::thread::spawn(move || {
        if let Ok(request) = resolver_login::signed("POST", "/confirm", &result.token, body.to_string().into_bytes()) {
            let _ = request.send();
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_pairs_bind_both_files_and_personal_proofs_bind_the_catalog_id() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let hit = |proof: serde_json::Value| {
            let mut data = serde_json::json!({"source":"Bandcamp","ticket":"1".repeat(32),"sha256":b});
            data.as_object_mut().unwrap().extend(proof.as_object().unwrap().clone());
            serde_json::from_value::<KnownCandidate>(data).unwrap()
        };
        let operator = hit(serde_json::json!({"baseline_sha256":a,"baseline_sha256s":[c]}));
        assert!(operator.matches("7", &a));
        assert!(operator.matches("7", &c));
        assert!(!operator.matches("7", &b));
        assert!(!operator.matches("7", &"d".repeat(64)));
        let commitment = pair_digest("7", &a, &b);
        let personal = hit(serde_json::json!({"pair_sha256":commitment}));
        assert!(personal.matches("7", &a));
        assert!(!personal.matches("8", &a));
        assert!(!personal.matches("7", &c));
        assert!(!hit(serde_json::json!({"baseline_sha256":a,"pair_sha256":commitment})).matches("7", &a));
        assert!(!hit(serde_json::json!({"baseline_sha256s":[a],"pair_sha256":commitment})).matches("7", &a));
    }

    #[test]
    fn ticket_response_requires_matching_bytes_or_a_previously_hashed_copy() {
        let response = |status, bytes: &[u8]| tungstenite::http::Response::builder()
            .status(status).header("content-length", bytes.len()).body(bytes.to_vec()).unwrap().into();
        let hash = media_digest(b"expected".as_slice()).unwrap();
        assert!(ticket_audio(response(200, b"tampered"), Some(&hash), None, &|| true).is_err());
        assert!(ticket_audio(response(304, b""), Some(&hash), None, &|| true).is_err());
        let buf = ticket_audio(response(200, b"expected"), Some(&hash), None, &|| true).unwrap();
        assert!(ticket_audio(response(403, b""), Some(&hash), Some(buf.clone()), &|| true).is_err());
        let cached = ticket_audio(response(304, b""), Some(&hash), Some(buf.clone()), &|| true).unwrap();
        assert!(std::sync::Arc::ptr_eq(&buf, &cached));
    }

    #[test]
    fn legacy_verified_media_is_bound_to_both_recordings_and_its_session() {
        let path = confirmed_audio::path("session", "7", "baseline A", "media A");
        assert_ne!(path, confirmed_audio::path("session", "7", "baseline B", "media A"));
        assert_ne!(path, confirmed_audio::path("session", "7", "baseline A", "media B"));
        assert_ne!(path, confirmed_audio::path("other session", "7", "baseline A", "media A"));
        assert_ne!(path, confirmed_audio::path("session", "8", "baseline A", "media A"));
    }

    #[test]
    fn only_public_catalog_tracks_produce_request_metadata() {
        let mut input = serde_json::json!({"id":42,"title":"Track", "artists":[{"id":1,"name":"Artist"}],"durationMs":1000});
        let metadata = |value: &serde_json::Value| public_metadata(&crate::api::parse_track(value).unwrap());
        assert_eq!(metadata(&input).unwrap(), serde_json::json!({"artists":["Artist"],"title":"track","duration_ms":1000}));
        input["id"] = "a-private-recording".into();
        assert!(metadata(&input).is_none());
        input["id"] = 42.into();
        input["trackSource"] = "UGC".into();
        assert!(metadata(&input).is_none());
    }

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
