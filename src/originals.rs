//! Resolve an audio-only substitution for an exact catalog track on the owner's account.

use crate::api::{Api, Res, Track, public_catalog_id};
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

const ACOUSTIC_POLICY: u8 = 3;
// Local cache revision changes when the verifier becomes stricter, independently of the wire policy.
const LOCAL_VERIFIER_REVISION: u8 = 5;

// Version 2 receipts already bind a restored-word check to the same byte pair.
// Newly sampled checks must only complete a version 3 lease.
fn supported_receipt(version: Option<u8>) -> bool {
    matches!(version, Some(2) | Some(ACOUSTIC_POLICY))
}

fn cacheable_verification(result: Option<f32>) -> Res<Option<f32>> {
    if result.is_none() && crate::verification::remote_speech::incomplete() {
        return Err("Проверка аудио не завершена".into());
    }
    Ok(result)
}

pub fn verified(api: &Api, original: &crate::verification::Recording, candidate: &crate::verification::Recording, lyrics: &crate::api::Lyrics, current: &impl Fn() -> bool) -> Option<f32> {
    if !current() || matches!(lyrics.omission_plan, crate::api::OmissionPlan::Incomplete)
        || (lyrics.synced.is_empty() && lyrics.text.is_empty()) { return None; }
    // A positive proof is specific to both decoded recordings, their order and the lyrics used.
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"ym-acoustic-proof\0");
    hash.update([ACOUSTIC_POLICY, LOCAL_VERIFIER_REVISION]);
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
    if let crate::api::OmissionPlan::Complete(checks) = &lyrics.omission_plan {
        // Include the complete plan as well as the acoustic policy in both cache keys.
        hash.update(b"\0ym-omission-review-v1\0");
        hash.update(serde_json::to_vec(checks).ok()?);
    }
    let key = format!("{:x}", hash.finalize());
    api.originals.verified.find(&key, || {
        // One record per bucket bounds the disk cache even for a long-lived installation.
        let dir = crate::config::cache_dir().join(format!("restored-speech-v{ACOUSTIC_POLICY}-r{LOCAL_VERIFIER_REVISION}"));
        let path = dir.join(&key[..2]);
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok((saved, time)) = serde_json::from_slice::<(String, f32)>(&bytes) {
                if saved == key && time.is_finite() && time >= 0.0 {
                    return Ok(Some(time));
                }
            }
        }
        let time = crate::verification::compare_while_current(original, candidate, lyrics, current);
        if !current() { return Err("Выбор трека изменился".into()); }
        if let Some(time) = time {
            let temp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::create_dir_all(&dir).is_ok() &&
                std::fs::write(&temp, serde_json::to_vec(&(&key, time)).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&temp, &path);
                let _ = std::fs::remove_file(&temp);
            }
        }
        cacheable_verification(time)
    })
}

/// A locally accepted edit is not automatically safe to publish to all users.
/// `verified` returns only the restoration position, so publication is limited to
/// a complete one-target omission plan whose sole target passed that verifier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VerificationOutcome {
    pub restored_at: f32,
    pub publishable: bool,
}

fn publishable_evidence(lyrics: &crate::api::Lyrics, restored_at: f32) -> bool {
    if !restored_at.is_finite() || restored_at <= 0.0 { return false; }
    let restored_ms = (restored_at * 1000.0).round();
    matches!(&lyrics.omission_plan, crate::api::OmissionPlan::Complete(checks)
        if checks.as_slice().first().is_some_and(|check| checks.len() == 1
            && check.missing.split_whitespace().count() == 1
            && restored_ms >= check.from_ms as f32 && restored_ms <= check.to_ms as f32))
}

pub fn verification_outcome(
    api: &Api, original: &crate::verification::Recording, candidate: &crate::verification::Recording,
    lyrics: &crate::api::Lyrics, current: &impl Fn() -> bool,
) -> Option<VerificationOutcome> {
    let restored_at = verified(api, original, candidate, lyrics, current)?;
    let publishable = publishable_evidence(lyrics, restored_at);
    Some(VerificationOutcome { restored_at, publishable })
}

pub fn can_match(track: &Track) -> bool {
    !track.user_upload && track.duration_ms > 0 && public_catalog_id(&track.id)
}

pub fn should_uncensor(
    api: &Api, track: &Track, text_language: impl FnOnce() -> Res<Option<bool>>,
    vocals: impl FnOnce() -> Option<bool>,
) -> bool {
    if !can_match(track) { return false; }
    let title = normalized(&track.title);
    if crate::language::russian_title(title_variant(&title).0) { return true; }
    api.originals.russian.find(&track.id, || {
        let language = if track.lyrics || title.chars().any(|c| matches!(c, 'а'..='я' | 'ё')) {
            match text_language() {
                Ok(Some(russian)) => Some(russian),
                Ok(None) => vocals(),
                Err(e) => return if vocals() == Some(true) { Ok(Some(true)) } else { Err(e) },
            }
        } else {
            // A Latin title and a missing catalog lyrics flag can still accompany Russian vocals.
            // Consult the exact public lyrics only when the local language probe is inconclusive.
            match vocals() {
                Some(russian) => Some(russian),
                None => text_language()?,
            }
        };
        language.map(Some).ok_or_else(|| "язык не определён".into())
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

fn already_credited_feature(track: &Track, version: &str) -> bool {
    let credit = version.strip_prefix("feat.").or_else(|| version.strip_prefix("featuring "));
    credit.is_some_and(|credit| {
        let name = credit.trim().replace('ё', "е");
        !name.is_empty() && track.artist_list.iter().any(|a| normalized(&a.name).replace('ё', "е") == name)
    })
}

fn recording_version(track: &Track) -> (String, Option<bool>) {
    let version = normalized(&track.version);
    if let Some(explicit) = variant(&version) { return (String::new(), Some(explicit)); }
    if already_credited_feature(track, &version) { return (String::new(), None); }
    (version, None)
}

fn catalog_version(track: &Track) -> (String, Option<bool>) {
    let (version, marker) = recording_version(track);
    // Yandex can list a guest after the edition, e.g. "Remix; feat. Guest".
    // Remove only a complete credit already present in the artist list. Keep
    // edition words and unknown credits; the complete artist IDs must also match.
    let version = version.split(';').map(str::trim)
        .filter(|part| !already_credited_feature(track, part)).collect::<Vec<_>>().join("; ");
    (version, marker)
}

fn catalog_candidates(track: &Track, candidates: &[Track]) -> Vec<String> {
    let title = normalized(&track.title);
    let (title, _) = title_variant(&title);
    let (stable_version, _) = catalog_version(track);
    let Some(recording_key) = recording(title, track.artist_list.iter().map(|a| a.id.clone())) else { return vec![] };
    let hits = candidates.iter().filter_map(|candidate| {
        if candidate.catalog_id() == track.catalog_id() || !can_match(candidate)
            || track.duration_ms.abs_diff(candidate.duration_ms) > MATCH_TOLERANCE_MS
        {
            return None;
        }
        let candidate_title = normalized(&candidate.title);
        let (candidate_title, marker) = title_variant(&candidate_title);
        let (candidate_version, version_marker) = catalog_version(candidate);
        let marker = version_marker.or(marker);
        let explicit = marker == Some(true)
            || (candidate.explicit && marker != Some(false));
        (explicit
            && stable_version == candidate_version
            && recording(candidate_title, candidate.artist_list.iter().map(|a| a.id.clone())).as_ref() == Some(&recording_key))
            .then(|| candidate.catalog_id().to_owned())
    });
    hits.collect()
}

pub fn catalog_versions(api: &Api, track: &Track) -> Vec<String> {
    let Some(query) = recording_query(track) else { return vec![] };
    api.originals.catalog.find(&track.id, || {
        let candidates = api.search_recording(&query)?;
        Ok(Some(catalog_candidates(track, &candidates)))
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

#[derive(Clone, Deserialize)]
pub struct OnlineCandidate {
    source: String,
    ticket: String,
    sha256: Option<String>,
    pub next: Option<String>,
    #[serde(default)]
    pub next_after_rejection: bool,
    #[serde(default)]
    speech: bool,
}

impl OnlineCandidate {
    pub(crate) fn needs_first_download(&self) -> bool { self.sha256.is_none() }

    fn valid_ticket(&self) -> bool {
        self.ticket.len() == 32 && self.ticket.bytes().all(|b| b.is_ascii_hexdigit())
    }

    fn valid(&self) -> bool {
        self.valid_ticket() && matches!(self.source.as_str(), "Bandcamp" | "SoundCloud" | "YouTube Music" | "Audius" | "Internet Archive")
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
    let (version, _) = recording_version(track);
    let title = if version.is_empty() { base.to_owned() } else { format!("{base} ({version})") };
    let artists: Vec<_> = track.artist_list.iter().map(|a| &a.name).collect();
    Some(serde_json::json!({"title":title,"artists":artists,"duration_ms":track.duration_ms}))
}

fn post_metadata(path: &str, token: &str, body: &serde_json::Value, timeout: Duration) -> Res<reqwest::blocking::Response> {
    let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let request = resolver_login::signed("POST", path, token, body)?;
    let request = if matches!(path, "/known" | "/confirm" | "/catalog/confirm" | "/known/review") {
        request.header("X-YM-Acoustic-Review", ACOUSTIC_POLICY.to_string())
    } else { request };
    let request = if path == "/search" { request.header("X-YM-Search-Pages", "1") } else { request };
    let request = if matches!(path, "/search" | "/next") { request.header("X-YM-Editions", "1") } else { request };
    let request = if matches!(path, "/search" | "/known") { request.header("X-YM-Netlabels", "1") } else { request };
    request.timeout(timeout).send().map_err(|e| e.to_string())
}

fn read_json<T: serde::de::DeserializeOwned>(response: reqwest::blocking::Response) -> Res<T> {
    use std::io::Read;
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    serde_json::from_reader(response.take(16 * 1024)).map_err(|e| e.to_string())
}

fn candidate_page(path: &str, mut post: impl FnMut() -> Res<reqwest::blocking::Response>) -> Res<Vec<OnlineCandidate>> {
    let started = Instant::now();
    let mut response = post()?;
    // A fast transient failure keeps the original owner-bound cursor. Re-sign
    // once with a new nonce, but do not double a slow upstream timeout. Denial
    // and ambiguous transport loss are never retried here.
    if path == "/next" && response.status().as_u16() == 503 && started.elapsed() < Duration::from_secs(1) {
        drop(response);
        response = post()?;
    }
    let hits: Vec<OnlineCandidate> = read_json(response)?;
    Ok(hits.into_iter().filter(OnlineCandidate::valid).take(12).collect())
}

fn candidates(path: &str, metadata: serde_json::Value) -> Res<Vec<OnlineCandidate>> {
    let Some(token) = resolver_login::credential() else { return Ok(vec![]) };
    candidate_page(path, || post_metadata(path, &token, &metadata, Duration::from_secs(24)))
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
    #[serde(default, deserialize_with = "read_proof_id")]
    proof_catalog_id: Option<String>,
    baseline_sha256: Option<String>,
    pair_sha256: Option<String>,
    #[serde(default)]
    pair_sha256s: Vec<String>,
    #[serde(default)]
    baseline_sha256s: Vec<String>,
    #[serde(default)]
    acoustic_pair_sha256s: Vec<String>,
    #[serde(default)]
    acoustic_review_version: Option<u8>,
}

fn read_proof_id<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<String>, D::Error> {
    let id = String::deserialize(value)?;
    if public_catalog_id(&id) { Ok(Some(id)) } else { Err(serde::de::Error::custom("invalid catalog id")) }
}

impl KnownCandidate {
    pub fn proof_id(&self) -> Option<&str> { self.proof_catalog_id.as_deref() }

    fn is_yandex(&self) -> bool { self.candidate.source == "Yandex Music" }

    fn valid(&self, track: &str) -> bool {
        let source_ok = if self.is_yandex() {
            self.candidate.valid_ticket() && self.pair_sha256.is_some()
        } else {
            self.candidate.valid()
        };
        source_ok && self.cache_key(track).is_some()
    }

    fn cache_key(&self, track: &str) -> Option<String> {
        let media = self.candidate.sha256.as_deref().filter(|s| digest_valid(s))?;
        if self.baseline_sha256s.len() > 16 || self.baseline_sha256s.iter().any(|s| !digest_valid(s) || s == media) { return None; }
        if self.pair_sha256s.len() > 8 || self.pair_sha256s.iter().enumerate().any(|(i, pair)|
            !digest_valid(pair) || pair == media || Some(pair) == self.pair_sha256.as_ref() || self.pair_sha256s[..i].contains(pair)
        ) { return None; }
        if !supported_receipt(self.acoustic_review_version) && !self.acoustic_pair_sha256s.is_empty() { return None; }
        if self.acoustic_pair_sha256s.len() > 9 || self.acoustic_pair_sha256s.iter().enumerate().any(|(i, pair)|
            !digest_valid(pair) || pair == media || self.acoustic_pair_sha256s[..i].contains(pair)
                || !(Some(pair) == self.pair_sha256.as_ref() || self.pair_sha256s.contains(pair)
                    || self.baseline_sha256.iter().chain(&self.baseline_sha256s).any(|s| pair_digest(track, s, media) == *pair))
        ) { return None; }
        match (&self.baseline_sha256, &self.pair_sha256) {
            (Some(baseline), None) if digest_valid(baseline) && baseline != media => Some(pair_digest(track, baseline, media)),
            (None, Some(pair)) if digest_valid(pair) && self.baseline_sha256s.is_empty() => Some(pair.clone()),
            _ => None,
        }
    }

    pub fn has_fast_proof(&self) -> bool {
        self.baseline_sha256.is_some() || !self.acoustic_pair_sha256s.is_empty()
    }

    pub fn has_local_review(&self, track: &str, token: &str) -> bool {
        self.cache_key(track).into_iter().chain(self.pair_sha256s.iter().cloned())
            .any(|pair| read_local_review(&local_review_path(token, &pair), &pair))
    }

    pub fn locally_reviewed(&self, track: &str, baseline: &str, token: &str) -> bool {
        if !self.matches(track, baseline) { return false; }
        let pair = pair_digest(track, baseline, self.candidate.sha256.as_deref().unwrap());
        read_local_review(&local_review_path(token, &pair), &pair)
    }

    pub fn remember_local_review(&self, track: &str, baseline: &str, token: &str) {
        if self.proof_id() != Some(track) || !self.matches(track, baseline) { return; }
        let pair = pair_digest(track, baseline, self.candidate.sha256.as_deref().unwrap());
        let _ = write_local_review(&local_review_path(token, &pair), &pair);
    }

    pub fn mixed_trust(&self) -> bool {
        self.pair_sha256.iter().chain(&self.pair_sha256s).any(|pair| !self.acoustic_pair_sha256s.contains(pair))
    }

    pub fn reviewed(&self, track: &str, baseline: &str) -> bool {
        self.baseline_sha256.as_deref() == Some(baseline) || self.baseline_sha256s.iter().any(|s| s == baseline)
            || (supported_receipt(self.acoustic_review_version) && self.candidate.sha256.as_deref().is_some_and(|media|
                self.acoustic_pair_sha256s.contains(&pair_digest(track, baseline, media))))
    }

    fn review_report(&self, track: &str, baseline: &str, redeemed_ticket: &str) -> Option<serde_json::Value> {
        if self.acoustic_review_version != Some(ACOUSTIC_POLICY) || self.proof_id() != Some(track) || !self.matches(track, baseline)
            || self.reviewed(track, baseline) || redeemed_ticket.len() != 32
            || !redeemed_ticket.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
        let pair = pair_digest(track, baseline, self.candidate.sha256.as_deref()?);
        Some(serde_json::json!({"ticket":redeemed_ticket,"pair_sha256":pair,"review_version":ACOUSTIC_POLICY}))
    }

    pub fn matches(&self, track: &str, baseline: &str) -> bool {
        let Some(media) = self.candidate.sha256.as_deref() else { return false };
        if !digest_valid(baseline) || media == baseline || self.cache_key(track).is_none() { return false; }
        if self.baseline_sha256.as_deref() == Some(baseline) || self.baseline_sha256s.iter().any(|s| s == baseline) {
            return true;
        }
        let expected = pair_digest(track, baseline, media);
        self.pair_sha256.as_ref() == Some(&expected) || self.pair_sha256s.contains(&expected)
    }
}

// Local acoustic decisions never authorize another account or bypass a fresh
// service lookup. The versioned, bounded cache names both exact encoded files.
fn local_review_path(token: &str, pair: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let owner = format!("{:x}", Sha256::digest(token.as_bytes()));
    crate::config::cache_dir().join(format!("reviewed-pairs-v{ACOUSTIC_POLICY}-r{LOCAL_VERIFIER_REVISION}")).join(owner).join(&pair[..2])
}

fn read_local_review(path: &std::path::Path, pair: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if metadata.len() != 64 || metadata.modified().ok().and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age > Duration::from_secs(7 * 86400)) { return false; }
    std::fs::read(path).is_ok_and(|bytes| bytes == pair.as_bytes())
}

fn write_local_review(path: &std::path::Path, pair: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| std::io::Error::other("review cache path"))?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), NEXT.fetch_add(1,Ordering::Relaxed)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = file.write_all(pair.as_bytes());
    drop(file);
    let result = result.and_then(|_| std::fs::rename(&temp, path));
    if result.is_err() { let _ = std::fs::remove_file(temp); }
    result
}

/// Only the current public catalog can establish that different queue and receipt IDs
/// identify the same recording. A title, duration or server-provided ID is insufficient.
pub fn same_proof_recording(track: &Track, proof: &str, get: impl FnOnce(&[String]) -> Res<Vec<Track>>) -> bool {
    if !can_match(track) || !public_catalog_id(proof) { return false; }
    if track.id == proof { return true; }
    let Ok(rows) = get(&[track.id.clone(), proof.into()]) else { return false };
    let find = |id: &str| rows.iter().find(|row| row.id == id && can_match(row));
    let (Some(playing), Some(cited)) = (find(&track.id), find(proof)) else { return false };
    playing.catalog_id() == cited.catalog_id()
        && (track.catalog_id() == track.id || track.catalog_id() == playing.catalog_id())
}

enum KnownLookup {
    Hit(Box<KnownCandidate>),
    Miss,
    Rejected,
    Retryable,
}

impl KnownLookup {
    fn retryable(&self) -> bool { matches!(self, Self::Retryable) }
    fn into_hit(self) -> Option<KnownCandidate> {
        match self { Self::Hit(hit) => Some(*hit), _ => None }
    }
}

#[derive(Clone, Copy)]
enum KnownPostError { Retryable, Terminal }

impl KnownPostError {
    fn lookup(self) -> KnownLookup {
        match self { Self::Retryable => KnownLookup::Retryable, Self::Terminal => KnownLookup::Miss }
    }
}

fn transient_io_kind(kind: std::io::ErrorKind) -> bool {
    matches!(kind, std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::TimedOut)
}

fn error_chain_has_transient_io(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if transient_io_kind(io.kind()) { return true; }
        }
        let Some(source) = error.source() else { return false; };
        error = source;
    }
}

fn reqwest_is_transient(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error_chain_has_transient_io(error)
}

fn post_known_metadata(token: &str, body: &serde_json::Value) -> Result<reqwest::blocking::Response, KnownPostError> {
    let body = serde_json::to_vec(body).map_err(|_| KnownPostError::Terminal)?;
    let request = resolver_login::signed("POST", "/known", token, body).map_err(|_| KnownPostError::Terminal)?;
    request.header("X-YM-Acoustic-Review", ACOUSTIC_POLICY.to_string())
        .header("X-YM-Netlabels", "1")
        .timeout(Duration::from_secs(4)).send()
        .map_err(|error| if reqwest_is_transient(&error) { KnownPostError::Retryable } else { KnownPostError::Terminal })
}

fn read_known_hits(response: reqwest::blocking::Response) -> Result<Vec<KnownCandidate>, KnownLookup> {
    if response.status() != reqwest::StatusCode::OK { return Err(known_response(&response)); }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(4096);
    if let Err(error) = response.take(16 * 1024).read_to_end(&mut bytes) {
        return Err(if error_chain_has_transient_io(&error) { KnownLookup::Retryable } else { KnownLookup::Miss });
    }
    serde_json::from_slice(&bytes).map_err(|_| KnownLookup::Miss)
}

fn retry_known_once(mut lookup: impl FnMut() -> KnownLookup, current: impl Fn() -> bool, on_retry: impl FnOnce(), mut wait: impl FnMut()) -> KnownLookup {
    let mut result = lookup();
    if result.retryable() && current() {
        // Bounded and cancellable; this is called only by the background known lookup.
        on_retry();
        for _ in 0..10 {
            if !current() { return KnownLookup::Miss; }
            wait();
        }
        if !current() { return KnownLookup::Miss; }
        result = lookup();
    }
    result
}

fn known_response(response: &reqwest::blocking::Response) -> KnownLookup {
    match response.status().as_u16() {
        502..=504 => KnownLookup::Retryable,
        401 | 403 => KnownLookup::Rejected,
        _ => KnownLookup::Miss,
    }
}

fn lookup_known_outcome(canonical: &str, original: &str, mut metadata: serde_json::Value, mut post: impl FnMut(&serde_json::Value) -> Result<reqwest::blocking::Response, KnownPostError>) -> KnownLookup {
    metadata["catalog_id"] = canonical.into();
    metadata["include_yandex"] = true.into();
    metadata["include_aliases"] = true.into();
    let mut response = match post(&metadata) { Ok(response) => response, Err(error) => return error.lookup() };
    let aliases = response.status() != reqwest::StatusCode::BAD_REQUEST;
    if !aliases {
        let Some(object) = metadata.as_object_mut() else { return KnownLookup::Miss };
        object.remove("include_aliases");
        response = match post(&metadata) { Ok(response) => response, Err(error) => return error.lookup() };
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            metadata.as_object_mut().unwrap().remove("include_yandex");
            response = match post(&metadata) { Ok(response) => response, Err(error) => return error.lookup() };
        }
    }
    let mut hits = match read_known_hits(response) { Ok(hits) => hits, Err(result) => return result };
    let mut requested = canonical;
    if !aliases && hits.is_empty() && original != canonical {
        // Older services indexed receipts only by the exact queue ID.
        requested = original;
        metadata["catalog_id"] = original.into();
        let response = match post(&metadata) { Ok(response) => response, Err(error) => return error.lookup() };
        hits = match read_known_hits(response) { Ok(hits) => hits, Err(result) => return result };
    }
    let [mut hit]: [KnownCandidate; 1] = match hits.try_into() { Ok(hit) => hit, Err(_) => return KnownLookup::Miss };
    if (!aliases && hit.proof_catalog_id.is_some()) || (hit.is_yandex() && metadata.get("include_yandex").is_none()) { return KnownLookup::Miss; }
    if !hit.valid(requested) { return KnownLookup::Miss; }
    hit.proof_catalog_id.get_or_insert_with(|| requested.to_owned());
    KnownLookup::Hit(Box::new(hit))
}

#[cfg(test)]
fn lookup_known(canonical: &str, original: &str, metadata: serde_json::Value, post: impl FnMut(&serde_json::Value) -> Result<reqwest::blocking::Response, KnownPostError>) -> Option<KnownCandidate> {
    lookup_known_outcome(canonical, original, metadata, post).into_hit()
}

pub fn known_candidate(track: &Track, token: &str, current: &impl Fn() -> bool) -> Option<KnownCandidate> {
    known_candidate_with_retry(track, token, current, || {})
}

pub fn known_candidate_with_retry(track: &Track, token: &str, current: &impl Fn() -> bool, on_retry: impl FnOnce()) -> Option<KnownCandidate> {
    let metadata = public_metadata(track)?;
    let lookup = || lookup_known_outcome(track.catalog_id(), &track.id, metadata.clone(), |body| {
        if !current() { return Err(KnownPostError::Terminal); }
        post_known_metadata(token, body)
    });
    // This runs on the lookup worker; baseline playback does not wait on it.
    // signed() creates a fresh timestamp and nonce for each actual HTTP attempt.
    let result = retry_known_once(lookup, current, on_retry, || std::thread::sleep(Duration::from_millis(20)));
    result.into_hit()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRedemption {
    alternative_id: String,
}

impl CatalogRedemption {
    fn checked_id(self, original: &str) -> Option<String> {
        (self.alternative_id != original && public_catalog_id(&self.alternative_id)).then_some(self.alternative_id)
    }
}

/// Hash only a complete encoded file. Both slow reporting and first-buffer verification
/// may be canceled while a Yandex stream is still arriving.
pub fn complete_digest(buf: &std::sync::Arc<crate::stream::Buf>, current: &impl Fn() -> bool) -> Option<String> {
    let limit = Instant::now() + Duration::from_secs(48);
    while current() && !buf.done() && Instant::now() < limit {
        buf.complete_before(Instant::now() + Duration::from_millis(200));
    }
    if !current() || !buf.complete_before(Instant::now()) { return None; }
    media_digest(buf.reader()).ok()
}

pub struct ExternalAudio {
    pub buf: std::sync::Arc<crate::stream::Buf>,
    pub format: String,
    pub cached: bool,
    token: String,
}

#[derive(Debug, PartialEq)]
enum AudioError { Transport, Busy, Denied, MissingTicket, DigestMismatch, Rejected }

impl AudioError {
    fn retryable(&self) -> bool { matches!(self, Self::Transport | Self::Busy) }
    fn label(&self) -> &'static str {
        match self {
            Self::Transport => "обрыв соединения",
            Self::Busy => "сервис временно недоступен",
            Self::Denied => "доступ отклонён",
            Self::MissingTicket => "билет недоступен",
            Self::DigestMismatch => "хеш аудио не совпал",
            Self::Rejected => "ответ отклонён",
        }
    }
}

fn fetch_audio(candidate: &OnlineCandidate, token: String, cached: Option<std::sync::Arc<crate::stream::Buf>>, timeout: Duration, current: &impl Fn() -> bool) -> Result<ExternalAudio, AudioError> {
    if !current() { return Err(AudioError::Rejected); }
    let mut request = resolver_login::signed("GET", &format!("/audio/{}", candidate.ticket), &token, vec![]).map_err(|_| AudioError::Rejected)?;
    if cached.is_some() {
        request = request.header("If-None-Match", format!("\"{}\"", candidate.sha256.as_deref().unwrap_or_default()));
    }
    let response = request.timeout(timeout).send().map_err(|error| {
        if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() { AudioError::Transport } else { AudioError::Rejected }
    })?;
    let reused = response.status() == reqwest::StatusCode::NOT_MODIFIED;
    let buf = ticket_audio(response, candidate.sha256.as_deref(), cached, current)?;
    Ok(ExternalAudio { buf, format: "MP3".into(), cached: reused, token })
}

fn ticket_audio(response: reqwest::blocking::Response, expected: Option<&str>, cached: Option<std::sync::Arc<crate::stream::Buf>>, current: &impl Fn() -> bool) -> Result<std::sync::Arc<crate::stream::Buf>, AudioError> {
    match response.status() {
        reqwest::StatusCode::NOT_MODIFIED => {
            let hash = expected.filter(|hash| digest_valid(hash)).ok_or(AudioError::Rejected)?;
            if response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()) != Some(&format!(r#""{hash}""#)) {
                return Err(AudioError::DigestMismatch);
            }
            cached.ok_or(AudioError::Rejected)
        }
        reqwest::StatusCode::OK => {
            let buf = crate::stream::Buf::from_media(response, current).map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionAborted => AudioError::Transport,
                _ => AudioError::Rejected,
            })?;
            if let Some(expected) = expected {
                if !digest_valid(expected) || confirmed_audio::digest(buf.reader()).ok().as_deref() != Some(expected) {
                    return Err(AudioError::DigestMismatch);
                }
            }
            Ok(buf)
        }
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Err(AudioError::Denied),
        reqwest::StatusCode::NOT_FOUND => Err(AudioError::MissingTicket),
        reqwest::StatusCode::BAD_GATEWAY | reqwest::StatusCode::SERVICE_UNAVAILABLE | reqwest::StatusCode::GATEWAY_TIMEOUT => Err(AudioError::Busy),
        _ => Err(AudioError::Rejected),
    }
}

fn cached_audio(token: &str, track: &str, media: &str, pair: &str, legacy_baseline: Option<&str>) -> Option<std::sync::Arc<crate::stream::Buf>> {
    let read = |key| confirmed_audio::read(&confirmed_audio::path(token, track, key, media), media);
    read(pair).or_else(|| legacy_baseline.and_then(read))
}

fn catalog_ticket_audio(
    response: reqwest::blocking::Response, original: &str, expected: &str, current: &impl Fn() -> bool,
    load_catalog: impl FnOnce(&str) -> Option<(std::sync::Arc<crate::stream::Buf>, String)>,
) -> Option<(std::sync::Arc<crate::stream::Buf>, String)> {
    if response.status() != reqwest::StatusCode::OK { return None; }
    let id = read_json::<CatalogRedemption>(response).ok()?.checked_id(original)?;
    if !current() { return None; }
    let (buf, format) = load_catalog(&id)?;
    (complete_digest(&buf, current)?.as_str() == expected).then_some((buf, format))
}

fn deliver_known<T>(
    hit: &KnownCandidate, current: &impl Fn() -> bool, refresh: impl FnOnce() -> Option<KnownCandidate>,
    mut fetch: impl FnMut(&OnlineCandidate) -> Result<T, AudioError>,
) -> Option<T> {
    if !current() { return None; }
    match fetch(&hit.candidate) {
        Ok(audio) => return Some(audio),
        Err(e) if e.retryable() && current() => crate::config::log(format!("Доставка проверенной версии: {}, повтор с новым билетом", e.label())),
        Err(e) => {
            if current() { crate::config::log(format!("Доставка проверенной версии: {}", e.label())); }
            return None;
        }
    }
    let next = refresh()?;
    if !current() || next.candidate.ticket == hit.candidate.ticket || next.candidate.source != hit.candidate.source
        || next.candidate.sha256 != hit.candidate.sha256 || next.proof_catalog_id != hit.proof_catalog_id
        || next.baseline_sha256 != hit.baseline_sha256 || next.baseline_sha256s != hit.baseline_sha256s
        || next.acoustic_review_version != hit.acoustic_review_version || next.acoustic_pair_sha256s != hit.acoustic_pair_sha256s
        || next.pair_sha256s != hit.pair_sha256s || next.pair_sha256 != hit.pair_sha256 { return None; }
    fetch(&next.candidate).inspect_err(|e| {
        if current() { crate::config::log(format!("Повторная доставка проверенной версии: {}", e.label())); }
    }).ok()
}

pub fn known_audio(
    hit: &KnownCandidate, track: &str, token: &str, needs_review: bool, current: &impl Fn() -> bool, refresh: impl FnOnce() -> Option<KnownCandidate>,
    load_catalog: impl FnOnce(&str) -> Option<(std::sync::Arc<crate::stream::Buf>, String)>,
) -> Option<(ExternalAudio, String)> {
    let media = hit.candidate.sha256.as_deref()?;
    let pair = hit.cache_key(track)?;
    if hit.is_yandex() {
        if !current() { return None; }
        let path = format!("/catalog/{}", hit.candidate.ticket);
        let response = resolver_login::signed("GET", &path, token, vec![]).ok()?
            .timeout(Duration::from_secs(4)).send().ok()?;
        let (buf, format) = catalog_ticket_audio(response, track, media, current, load_catalog)?;
        return Some((ExternalAudio { buf, format, cached: false, token: token.into() }, hit.candidate.ticket.clone()));
    }
    let cached = if needs_review && hit.acoustic_review_version == Some(ACOUSTIC_POLICY) {
        // This lease becomes usable only after a full delivery, including when local ASR is cached.
        None
    } else {
        cached_audio(token, track, media, &pair, hit.baseline_sha256.as_deref()).or_else(|| {
            hit.baseline_sha256s.iter().find_map(|s| cached_audio(token, track, media, &pair_digest(track, s, media), Some(s)))
        })
    };
    // First-buffer waiting has its own short deadline. Finish an already matched
    // download in the background even when a slow link takes more than 15 seconds.
    deliver_known(hit, current, refresh, |ticket| {
        let media = fetch_audio(ticket, token.into(), cached.clone(), Duration::from_secs(48), current)?;
        Ok((media, ticket.ticket.clone()))
    })
}

pub fn remember_known_review(hit: &KnownCandidate, track: &str, baseline: &str, redeemed_ticket: &str, token: &str) {
    let Some(report) = hit.review_report(track, baseline, redeemed_ticket) else { return };
    let token = token.to_owned();
    let revision = resolver_login::session_revision();
    std::thread::spawn(move || {
        if resolver_login::session_revision() != revision || !resolver_login::mode_enabled()
            || !crate::api::allow_explicit() || resolver_login::credential().as_ref() != Some(&token) { return; }
        send_acoustic_review(&token, &report, &|| resolver_login::session_revision() == revision
            && resolver_login::mode_enabled() && crate::api::allow_explicit());
    });
}

pub fn cache_known(hit: &KnownCandidate, track: &str, audio: &ExternalAudio) {
    if audio.cached || hit.is_yandex() { return; }
    if let (Some(pair), Some(media)) = (hit.cache_key(track), &hit.candidate.sha256) {
        let path = confirmed_audio::path(&audio.token, track, &pair, media);
        let buf = audio.buf.clone();
        std::thread::spawn(move || { let _ = confirmed_audio::store(&path, &buf); });
    }
}

pub(crate) fn known_speech(hit: &KnownCandidate, media: &ExternalAudio, redeemed_ticket: &str) -> Option<std::sync::Arc<crate::verification::remote_speech::Provider>> {
    // A delivery retry may use a fresh ticket. Only the redeemed ticket owns the
    // server's byte receipt; Yandex candidates are rejected by candidate_speech.
    let mut candidate = hit.candidate.clone();
    candidate.ticket = redeemed_ticket.to_owned();
    candidate_speech(&candidate,media)
}

pub(crate) fn candidate_speech(candidate: &OnlineCandidate, result: &ExternalAudio) -> Option<std::sync::Arc<crate::verification::remote_speech::Provider>> {
    if !candidate.speech || !candidate.valid() || !result.buf.done() { return None; }
    let sha256 = media_digest(result.buf.reader()).ok()?;
    let ticket = candidate.ticket.clone();
    let token = result.token.clone();
    let revision = resolver_login::session_revision();
    Some(crate::verification::remote_speech::Provider::new(move |ranges,timeout| {
        if resolver_login::session_revision() != revision || resolver_login::credential().as_deref() != Some(&token) { return None; }
        let clips: Vec<_> = ranges.iter().map(|&(from_ms,to_ms)| serde_json::json!({"from_ms":from_ms,"to_ms":to_ms})).collect();
        let body = serde_json::json!({"ticket":ticket,"sha256":sha256,"clips":clips});
        let response = post_metadata("/speech", &token, &body, timeout).ok()?.error_for_status().ok()?;
        use std::io::Read;
        #[derive(Deserialize)]
        struct Reply { model: String, sha256: String, clips: Vec<crate::verification::remote_speech::Transcript> }
        let reply: Reply = serde_json::from_reader(response.take(65536)).ok()?;
        if reply.model != "gigaam-v4-rnnt-rr-timed-v1" || reply.sha256 != sha256
            || resolver_login::session_revision() != revision { return None; }
        Some(reply.clips)
    }))
}

pub fn online_audio(candidate: &OnlineCandidate, track: &Track, original: Option<&str>, current: &impl Fn() -> bool) -> Res<ExternalAudio> {
    let token = resolver_login::credential().ok_or("Сначала подключите внешние источники")?;
    let cached = original.zip(candidate.sha256.as_deref()).and_then(|(baseline, media)| {
        if !digest_valid(media) { return None; }
        let read = |id| cached_audio(&token, id, media, &pair_digest(id, baseline, media), Some(baseline));
        read(track.catalog_id()).or_else(|| if track.catalog_id() != track.id { read(&track.id) } else { None })
    });
    fetch_audio(candidate, token, cached, Duration::from_secs(48), current).map_err(|e| e.label().into())
}

fn catalog_confirmation(track: &Track, alternative: &str, baseline: &str, media: &str) -> Option<serde_json::Value> {
    if !public_catalog_id(alternative) || alternative == track.catalog_id() || alternative == track.id || baseline == media || !digest_valid(baseline) || !digest_valid(media) {
        return None;
    }
    let mut body = public_metadata(track)?;
    body["catalog_id"] = track.catalog_id().into();
    body["alternative_id"] = alternative.into();
    body["sha256"] = media.into();
    body["pair_sha256"] = pair_digest(track.catalog_id(), baseline, media).into();
    Some(body)
}

/// The initial confirmation has the .4 body. Only an upgraded service can issue a
/// fresh owner-bound review lease after it has accepted that exact recording.
fn confirm_acoustic_pair(path: &str, token: &str, body: &serde_json::Value, current: &impl Fn() -> bool) {
    if !current() { return; }
    // A server predating v3 rejects this field instead of recording a sampled
    // check as a legacy confirmation. The signature commits to the policy in the body.
    let mut confirmation = body.clone();
    confirmation["acoustic_policy"] = ACOUSTIC_POLICY.into();
    let Ok(response) = post_metadata(path, token, &confirmation, Duration::from_secs(8)) else { return };
    if response.status() != reqwest::StatusCode::OK || !current() { return; }
    let Ok(lease) = read_json::<serde_json::Value>(response) else { return };
    let Some(ticket) = lease["ticket"].as_str() else { return };
    let Some(pair) = body["pair_sha256"].as_str() else { return };
    if lease["review_version"].as_u64() != Some(u64::from(ACOUSTIC_POLICY)) || lease["pair_sha256"] != pair
        || ticket.len() != 32 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) || !digest_valid(pair) { return; }
    send_acoustic_review(token, &lease, current);
}

fn send_acoustic_review(token: &str, body: &serde_json::Value, current: &impl Fn() -> bool) {
    for _ in 0..2 {
        if !current() { return; }
        match post_metadata("/known/review", token, body, Duration::from_secs(8)) {
            Ok(response) if !response.status().is_server_error() => return,
            _ => {}
        }
    }
}

pub fn remember_catalog(
    track: &Track, alternative: String, baseline: std::sync::Arc<crate::stream::Buf>,
    media: std::sync::Arc<crate::stream::Buf>, revision: u64, publishable: bool,
) {
    if !publishable { return; }
    let Some(token) = resolver_login::credential() else { return };
    let track = track.clone();
    std::thread::spawn(move || {
        let authorized = || resolver_login::session_revision() == revision && resolver_login::mode_enabled() && crate::api::allow_explicit();
        let Some(baseline) = complete_digest(&baseline, &authorized) else { return };
        let Some(media) = complete_digest(&media, &authorized) else { return };
        let Some(body) = catalog_confirmation(&track, &alternative, &baseline, &media) else { return };
        if !authorized() || resolver_login::credential().as_deref() != Some(&token) { return; }
        confirm_acoustic_pair("/catalog/confirm", &token, &body, &authorized);
    });
}

pub fn remember_external(result: ExternalAudio, candidate: &OnlineCandidate, track: &str, original: Option<&str>, publishable: bool) {
    let revision = resolver_login::session_revision();
    let Ok(digest) = confirmed_audio::digest(result.buf.reader()) else { return };
    let mut body = serde_json::json!({"ticket":candidate.ticket,"sha256":digest});
    if let Some(original) = original {
        let proof = pair_digest(track, original, &digest);
        let path = confirmed_audio::path(&result.token, track, &proof, &digest);
        if !result.cached || !path.exists() { let _ = confirmed_audio::store(&path, &result.buf); }
        let _ = write_local_review(&local_review_path(&result.token, &proof), &proof);
        body["catalog_id"] = track.into();
        body["pair_sha256"] = proof.into();
    }
    if original.is_none() { return; }
    std::thread::spawn(move || {
        let current = || resolver_login::session_revision() == revision
            && resolver_login::credential().as_deref() == Some(&result.token)
            && resolver_login::mode_enabled() && crate::api::allow_explicit();
        if !current() { return; }
        if publishable {
            confirm_acoustic_pair("/confirm", &result.token, &body, &current);
        } else {
            // This owner's checked byte pair enables replay without making it a shared verdict.
            body["local_only"] = true.into();
            body["acoustic_policy"] = ACOUSTIC_POLICY.into();
            for _ in 0..2 {
                if !current() { return; }
                match post_metadata("/confirm", &result.token, &body, Duration::from_secs(8)) {
                    Ok(response) if !response.status().is_server_error() => return,
                    _ => {}
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_assistance_does_not_cache_a_negative_and_recovery_is_cached() {
        use crate::verification::remote_speech::{Provider, Transcript, with_candidate, transcript};
        let cache = LookupCache::<f32>::default();
        let pcm = vec![0i16; 16000];
        with_candidate(&pcm, Provider::new(|_, _| None), || {
            assert_eq!(cache.find("pair", || {
                assert!(transcript(&pcm).is_none());
                assert!(crate::verification::remote_speech::retryable_failure());
                cacheable_verification(None)
            }), None);
        });
        with_candidate(&pcm, Provider::new(|ranges, _| Some(ranges.iter().map(|&(from_ms,to_ms)|
            Transcript { from_ms,to_ms,text:"restored".into(),words:vec![] }).collect())), || {
            assert_eq!(cache.find("pair", || {
                assert!(transcript(&pcm).is_some());
                assert!(!crate::verification::remote_speech::incomplete());
                cacheable_verification(Some(1.0))
            }), Some(1.0));
        });
        assert_eq!(cache.find("pair", || panic!("recovered positive must be cached")), Some(1.0));
    }

    #[test]
    fn definitive_negative_and_successful_local_fallback_remain_cached() {
        use crate::verification::remote_speech::{Provider, with_candidate, transcript};
        let cache = LookupCache::<f32>::default();
        assert_eq!(cache.find("negative", || cacheable_verification(None)), None);
        assert_eq!(cache.find("negative", || panic!("definitive negative must be cached")), None);
        let pcm = vec![0i16; 16000];
        with_candidate(&pcm, Provider::new(|_, _| None), || {
            assert_eq!(cache.find("local-proof", || {
                assert!(transcript(&pcm).is_none());
                cacheable_verification(Some(2.0))
            }), Some(2.0));
        });
        assert_eq!(cache.find("local-proof", || panic!("local positive must be cached")), Some(2.0));
        assert!(!crate::verification::remote_speech::incomplete(), "failure must not leak out of its candidate scope");
    }

    #[test]
    fn only_one_complete_positive_omission_check_is_publishable() {
        use crate::api::{OmissionCheck, OmissionPlan};
        let check = OmissionCheck {
            from_ms: 10_000, to_ms: 12_000, missing: "слово".into(),
            before: ["одно".into(), "два".into()], after: ["три".into(), "четыре".into()],
        };
        let mut lyrics = crate::api::Lyrics { text: vec!["обычный несинхронный текст".into()], ..Default::default() };
        assert!(!publishable_evidence(&lyrics, 11.0), "plain text alone is local-only");
        lyrics.omission_plan = OmissionPlan::Complete(vec![]);
        assert!(!publishable_evidence(&lyrics, 11.0), "an empty plan does not establish coverage");
        lyrics.omission_plan = OmissionPlan::Complete(vec![check.clone()]);
        assert!(publishable_evidence(&lyrics, 11.0), "a positive target within the sole complete omission window is publishable");
        assert!(!publishable_evidence(&lyrics, 9.9), "a restoration before the target window is local-only");
        assert!(!publishable_evidence(&lyrics, 12.1), "a restoration after the target window is local-only");
        if let OmissionPlan::Complete(checks) = &mut lyrics.omission_plan { checks[0].missing = "слово тест".into(); }
        assert!(!publishable_evidence(&lyrics, 11.0), "a multiword target needs richer coverage data");
        lyrics.omission_plan = OmissionPlan::Complete(vec![check.clone(), check]);
        assert!(!publishable_evidence(&lyrics, 11.0), "the current verifier result does not identify coverage for multiple targets");
        lyrics.omission_plan = OmissionPlan::Incomplete;
        assert!(!publishable_evidence(&lyrics, 11.0));
        lyrics.omission_plan = OmissionPlan::Complete(vec![OmissionCheck {
            from_ms: 10_000, to_ms: 12_000, missing: "слово".into(),
            before: ["одно".into(), "два".into()], after: ["три".into(), "четыре".into()],
        }]);
        assert!(!publishable_evidence(&lyrics, f32::NAN));
        assert!(!publishable_evidence(&lyrics, 0.0));
    }

    #[test]
    fn saved_fallback_hint_is_optional_on_a_candidate_page() {
        let mut row = serde_json::json!({"source":"Bandcamp","ticket":"1".repeat(32),"next":"2".repeat(32)});
        let read = |row| serde_json::from_value::<OnlineCandidate>(row).unwrap();
        assert!(!read(row.clone()).next_after_rejection);
        row["next_after_rejection"] = true.into();
        let candidate = read(row);
        assert!(candidate.next_after_rejection && candidate.valid());
        assert_eq!(candidate.next.unwrap(), "2".repeat(32));
    }

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
    #[ignore = "needs licensed local audio, lyrics, a test-only profile and the bundled speech model"]
    fn licensed_versioned_review_reuses_only_a_current_positive() {
        use sha2::{Digest, Sha256};
        let dir = std::path::PathBuf::from(std::env::var_os("YM_ACOUSTIC_FIXTURE").unwrap());
        assert_eq!(crate::config::dir(), std::path::PathBuf::from(std::env::var_os("YM_TEST_PROFILE").unwrap()));
        let read = |name| {
            let decoder = rodio::Decoder::new(std::io::BufReader::new(std::fs::File::open(dir.join(name)).unwrap())).unwrap();
            crate::verification::Recording::decode(decoder, 218_070).unwrap()
        };
        let original = read("media/trial-3/two-silence.mp3");
        let alternative = read("media/intact.mp3");
        let row: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("reference.json")).unwrap()).unwrap();
        let lyrics = crate::api::reference_row(&row, 218_070);
        assert!(lyrics.synced.is_empty() && !lyrics.text.is_empty());
        // Reconstruct the real old cache key for the (incorrect) reverse direction.
        let mut old = Sha256::new();
        old.update(alternative.digest);
        old.update(original.digest);
        for line in &lyrics.text { old.update((line.len() as u64).to_le_bytes()); old.update(line.as_bytes()); }
        let old = format!("{:x}", old.finalize());
        let v1 = crate::config::cache_dir().join("restored-speech-v1");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::write(v1.join(&old[..2]), serde_json::to_vec(&(&old, 42.0)).unwrap()).unwrap();
        let api = Api::new("");
        let start = Instant::now();
        assert!(verified(&api,&alternative,&original,&lyrics,&|| true).is_none());
        let actual = verified(&api,&original,&alternative,&lyrics,&|| true).unwrap();
        assert!((actual - 43.88).abs() < 0.5);
        let fresh = start.elapsed();
        let hits = std::cell::Cell::new(0);
        let start = Instant::now();
        // A new API object forces the persistent cache path; fresh ASR calls current repeatedly.
        let cached = verified(&Api::new(""),&original,&alternative,&lyrics,&|| { hits.set(hits.get()+1); true }).unwrap();
        assert_eq!(cached,actual);
        assert_eq!(hits.get(),1);
        eprintln!("trigger={actual:.3}s reverse_and_positive={fresh:?} new_profile_object_hits={} persistent_read={:?}",hits.get(),start.elapsed());
    }

    #[test]
    fn old_shared_receipts_never_serve_as_current_acoustic_proofs() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let pair = pair_digest("7", &a, &b);
        let mut row = serde_json::json!({"source":"Bandcamp","ticket":"1".repeat(32),"sha256":b,"proof_catalog_id":"7","pair_sha256":pair,
            "known_review":true,"reviewed_pair_sha256s":[pair]});
        let old: KnownCandidate = serde_json::from_value(row.clone()).unwrap();
        assert!(old.matches("7", &a));
        assert!(!old.reviewed("7", &a));
        assert!(old.review_report("7", &a, &"1".repeat(32)).is_none());
        row["acoustic_pair_sha256s"] = serde_json::json!([pair]);
        assert!(!serde_json::from_value::<KnownCandidate>(row).unwrap().valid("7"));
    }

    #[test]
    fn upgrade_uses_redeemed_ticket_and_only_the_offered_unreviewed_pair() {
        let operator = "a".repeat(64);
        let old = "b".repeat(64);
        let media = "c".repeat(64);
        let pair = pair_digest("17", &old, &media);
        let row = serde_json::json!({
            "source":"Bandcamp", "ticket":"1".repeat(32), "sha256":media, "proof_catalog_id":"17",
            "baseline_sha256":operator, "baseline_sha256s":["d".repeat(64)], "pair_sha256s":[pair], "acoustic_review_version":ACOUSTIC_POLICY
        });
        let read = |row: serde_json::Value| serde_json::from_value::<KnownCandidate>(row).unwrap();
        let mut earlier = row.clone();
        earlier["acoustic_review_version"] = 2.into();
        assert!(read(earlier).review_report("17", &old, &"1".repeat(32)).is_none());
        let hit = read(row.clone());
        for baseline in [&operator, &"d".repeat(64)] {
            assert!(hit.matches("17", baseline) && hit.reviewed("17", baseline));
            assert!(hit.review_report("17", baseline, &hit.candidate.ticket).is_none());
        }
        let mut refreshed = row.clone();
        refreshed["ticket"] = "2".repeat(32).into();
        let redeemed = deliver_known(&hit, &|| true, || Some(read(refreshed)), |candidate| {
            if candidate.ticket == hit.candidate.ticket { return Err(AudioError::Transport); }
            Ok(candidate.ticket.clone())
        }).unwrap();
        let report = serde_json::json!({"ticket":"2".repeat(32), "pair_sha256":pair, "review_version":ACOUSTIC_POLICY});
        assert_eq!(hit.review_report("17", &old, &redeemed), Some(report.clone()));
        assert!(hit.review_report("18", &old, &redeemed).is_none());
        assert!(hit.review_report("17", &"e".repeat(64), &redeemed).is_none());
        assert!(hit.review_report("17", &old, "invalid").is_none());
        let mut shared = row;
        shared.as_object_mut().unwrap().remove("baseline_sha256");
        shared.as_object_mut().unwrap().remove("baseline_sha256s");
        shared.as_object_mut().unwrap().remove("pair_sha256s");
        shared["source"] = "Yandex Music".into();
        shared["pair_sha256"] = pair.clone().into();
        assert_eq!(read(shared.clone()).review_report("17", &old, &redeemed), Some(report));
        shared["acoustic_pair_sha256s"] = serde_json::json!([pair]);
        assert!(read(shared.clone()).review_report("17", &old, &redeemed).is_none());
        shared.as_object_mut().unwrap().remove("acoustic_review_version");
        shared.as_object_mut().unwrap().remove("acoustic_pair_sha256s");
        assert!(read(shared).review_report("17", &old, &redeemed).is_none());
    }

    #[test]
    fn alternate_commitments_still_bind_each_quality_to_the_exact_media_and_id() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let media = "c".repeat(64);
        let first = pair_digest("17", &a, &media);
        let second = pair_digest("17", &b, &media);
        let row = serde_json::json!({"source":"YouTube Music","ticket":"1".repeat(32),"sha256":media,
                                     "pair_sha256":first,"pair_sha256s":[second]});
        let hit: KnownCandidate = serde_json::from_value(row.clone()).unwrap();
        assert!(hit.matches("17", &a) && hit.matches("17", &b));
        assert!(!hit.matches("18", &b) && !hit.matches("17", &"d".repeat(64)));
        assert!(!hit.reviewed("17", &a));
        let mut review = row.clone();
        review["acoustic_review_version"] = 2.into();
        review["acoustic_pair_sha256s"] = serde_json::json!([second]);
        let approved = serde_json::from_value::<KnownCandidate>(review.clone()).unwrap();
        assert!(approved.valid("17") && approved.reviewed("17", &b));
        assert!(!approved.reviewed("17", &a) && !approved.reviewed("18", &b));
        review["acoustic_pair_sha256s"] = serde_json::json!(["d".repeat(64)]);
        assert!(!serde_json::from_value::<KnownCandidate>(review).unwrap().valid("17"));
        let mut different = row.clone();
        different["sha256"] = "d".repeat(64).into();
        assert!(!serde_json::from_value::<KnownCandidate>(different).unwrap().matches("17", &b));
        for invalid in [vec![first.clone()], vec![second.clone(), second.clone()], vec![media.clone()],
                        vec!["bad".into()], (0..9).map(|n| format!("{n:064x}")).collect()] {
            let mut value = row.clone();
            value["pair_sha256s"] = serde_json::json!(invalid);
            assert!(!serde_json::from_value::<KnownCandidate>(value).unwrap().valid("17"));
        }
        let mut operator = row;
        operator.as_object_mut().unwrap().remove("pair_sha256");
        operator["baseline_sha256"] = a.clone().into();
        let operator: KnownCandidate = serde_json::from_value(operator).unwrap();
        assert!(operator.matches("17", &a) && operator.matches("17", &b));
    }

    fn json_response(status: u16, body: &serde_json::Value) -> reqwest::blocking::Response {
        tungstenite::http::Response::builder().status(status).body(body.to_string().into_bytes()).unwrap().into()
    }

    #[test]
    fn known_retries_only_an_old_servers_400_and_keeps_yandex_out_of_external_audio() {
        let metadata = serde_json::json!({"title":"song","artists":["A"],"duration_ms":1000});
        let data = serde_json::json!([{"source":"Yandex Music","ticket":"1".repeat(32),"sha256":"a".repeat(64),"pair_sha256":"b".repeat(64)}]);
        let hit = lookup_known("7", "7", metadata.clone(), |_| Ok(json_response(200, &data))).unwrap();
        assert!(hit.is_yandex() && !hit.candidate.valid());
        let mut external = data.clone();
        external[0]["source"] = "Bandcamp".into();
        let mut sent = vec![];
        let old = lookup_known("7", "7", metadata.clone(), |body| {
            sent.push(body.clone());
            Ok(if sent.len() <= 2 { json_response(400, &serde_json::Value::Null) } else { json_response(200, &external) })
        }).unwrap();
        assert!(old.candidate.valid());
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].as_object_mut().unwrap().remove("include_aliases"), Some(true.into()));
        assert_eq!(sent[0], sent[1]);
        assert_eq!(sent[1].as_object_mut().unwrap().remove("include_yandex"), Some(true.into()));
        assert_eq!(sent[1], sent[2]);
        assert_eq!(sent[1]["catalog_id"], "7");
        for status in [401,403,404,500] {
            let mut attempts = 0;
            assert!(lookup_known("7", "7", metadata.clone(), |_| { attempts += 1; Ok(json_response(status, &data)) }).is_none());
            assert_eq!(attempts, 1);
        }
        let mut operator_only = data;
        operator_only[0].as_object_mut().unwrap().remove("pair_sha256");
        operator_only[0]["baseline_sha256"] = "b".repeat(64).into();
        assert!(lookup_known("7", "7", metadata, |_| Ok(json_response(200, &operator_only))).is_none());
    }

    #[test]
    fn known_retries_one_transient_failure_but_not_empty_or_revoked_sessions() {
        let metadata = serde_json::json!({"title":"song","artists":["A"],"duration_ms":1000});
        let data = serde_json::json!([{"source":"Bandcamp","ticket":"1".repeat(32),"sha256":"a".repeat(64),"baseline_sha256":"b".repeat(64)}]);
        for transient in [None, Some(502), Some(503), Some(504)] {
            let mut attempts = 0;
            let hit = retry_known_once(|| {
                attempts += 1;
                lookup_known_outcome("7", "7", metadata.clone(), |_| {
                    if attempts == 1 {
                        match transient {
                            Some(status) => Ok(json_response(status, &serde_json::Value::Null)),
                            None => Err(KnownPostError::Retryable),
                        }
                    } else { Ok(json_response(200, &data)) }
                })
            }, || true, || {}, || {});
            assert!(matches!(hit, KnownLookup::Hit(_)));
            assert_eq!(attempts, 2, "each transient failure gets exactly one retry");
        }

        {
            let post_error = KnownPostError::Terminal;
            let mut attempts = 0;
            let outcome = retry_known_once(|| {
                attempts += 1;
                lookup_known_outcome("7", "7", metadata.clone(), |_| Err(post_error))
            }, || true, || panic!("signing/other errors must not retry"), || panic!("terminal errors must not wait"));
            assert_eq!(attempts, 1);
            assert!(matches!(outcome, KnownLookup::Miss));
        }

        for (status, body) in [(200, serde_json::json!([])), (401, serde_json::Value::Null), (403, serde_json::Value::Null)] {
            let mut attempts = 0;
            let outcome = retry_known_once(|| {
                attempts += 1;
                lookup_known_outcome("7", "7", metadata.clone(), |_| Ok(json_response(status, &body)))
            }, || true, || panic!("terminal outcomes must not wait"), || panic!("terminal outcomes must not wait"));
            assert_eq!(attempts, 1);
            match outcome {
                KnownLookup::Miss => {},
                KnownLookup::Rejected => {},
                _ => panic!("empty result and revoked/unauthorized sessions are terminal"),
            }
        }
    }

    #[test]
    fn known_retries_only_transient_io_error_kinds() {
        for kind in [std::io::ErrorKind::ConnectionReset, std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe, std::io::ErrorKind::UnexpectedEof, std::io::ErrorKind::TimedOut] {
            assert!(error_chain_has_transient_io(&std::io::Error::new(kind, "socket closed")));
        }
        assert!(!error_chain_has_transient_io(&std::io::Error::new(std::io::ErrorKind::InvalidData, "bad json")));
    }

    #[test]
    fn continuation_retries_only_one_explicit_service_failure() {
        let good = r#"[{"source":"YouTube Music","ticket":"0123456789abcdef0123456789abcdef"}]"#;
        let response = |status, body: &str| tungstenite::http::Response::builder()
            .status(status).body(body.as_bytes().to_vec()).unwrap().into();
        let mut calls = 0;
        let found = candidate_page("/next", || {
            calls += 1;
            Ok(if calls == 1 { response(503, "") } else { response(200, good) })
        }).unwrap();
        assert_eq!(calls, 2);
        assert_eq!(found.len(), 1);
        for (path, status, count) in [("/next",503,2),("/search",503,1),("/next",401,1),("/next",403,1),("/next",404,1),("/next",409,1)] {
            let mut calls = 0;
            assert!(candidate_page(path, || { calls += 1; Ok(response(status, "")) }).is_err());
            assert_eq!(calls, count);
        }
        let mut calls = 0;
        assert!(candidate_page("/next", || { calls += 1; Err("response lost".into()) }).is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn continuation_does_not_repeat_a_slow_upstream_failure() {
        let mut calls = 0;
        let result = candidate_page("/next", || {
            calls += 1;
            std::thread::sleep(Duration::from_millis(1100));
            Ok(tungstenite::http::Response::builder().status(503).body(Vec::<u8>::new()).unwrap().into())
        });
        assert!(result.is_err());
        assert_eq!(calls, 1, "a timed-out provider must not start a second slow search");
    }

    #[test]
    fn known_truncated_metadata_body_is_retryable_transport_but_bad_json_is_terminal() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut input = std::io::BufReader::new(&stream);
            let mut line = String::new();
            loop {
                line.clear();
                if input.read_line(&mut line).unwrap() == 0 || line == "\r\n" { break; }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n[]").unwrap();
            stream.flush().unwrap();
        });
        let response = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(2))
            .build().unwrap().get(endpoint).send().unwrap();
        peer.join().unwrap();
        assert!(matches!(read_known_hits(response), Err(KnownLookup::Retryable)));

        let malformed = tungstenite::http::Response::builder().status(200).body(b"{".to_vec()).unwrap().into();
        assert!(matches!(read_known_hits(malformed), Err(KnownLookup::Miss)));
    }

    #[test]
    fn known_retry_stops_when_track_is_cancelled() {
        let mut attempts = 0;
        let active = std::cell::Cell::new(true);
        let outcome = retry_known_once(|| {
            attempts += 1;
            KnownLookup::Retryable
        }, || active.get(), || {}, || active.set(false));
        assert_eq!(attempts, 1);
        assert!(matches!(outcome, KnownLookup::Miss));
    }

    #[test]
    fn receipt_alias_requires_both_catalog_rows_and_the_original_pair_id() {
        let make = |id: &str, real: &str| crate::api::parse_track(&serde_json::json!({
            "id": id, "realId": real, "title": "Song", "durationMs": 120000, "artists": [{"id": 1, "name": "Artist"}]
        })).unwrap();
        for (old, canonical) in [("57878398", "148716915"), ("113820638", "148715960")] {
            let queue = make(old, canonical);
            assert_eq!(queue.id, old);
            assert_eq!(queue.catalog_id(), canonical);
            let alt = "999";
            let (baseline, media) = ("a".repeat(64), "b".repeat(64));
            let new = catalog_confirmation(&queue, alt, &baseline, &media).unwrap();
            assert_eq!(new["catalog_id"], canonical);
            assert_eq!(new["pair_sha256"], pair_digest(canonical, &baseline, &media));
            let metadata = public_metadata(&queue).unwrap();
            let response = serde_json::json!([{"source":"Bandcamp","ticket":"f".repeat(32),
                "sha256":media,"proof_catalog_id":old,"pair_sha256":pair_digest(old, &baseline, &media)}]);
            let hit = lookup_known(canonical, canonical, metadata.clone(), |body| {
                assert_eq!(body["include_aliases"], true);
                assert_eq!(body["catalog_id"], canonical);
                Ok(json_response(200, &response))
            }).unwrap();
            assert_eq!(hit.proof_id(), Some(old));
            let playing = make(canonical, canonical);
            assert!(same_proof_recording(&playing, old, |ids| {
                assert_eq!(ids, [canonical, old]);
                Ok(vec![queue.clone(), playing.clone()])
            }));
            assert!(hit.matches(old, &baseline));
            assert!(!hit.matches(canonical, &baseline));
            assert!(!hit.matches(old, &"c".repeat(64)));
            let other_edition = make(old, "555");
            assert!(!same_proof_recording(&playing, old, |_| Ok(vec![playing.clone(), other_edition])));
            assert!(!same_proof_recording(&playing, old, |_| Ok(vec![playing.clone()])));
            assert!(same_proof_recording(&playing, canonical, |_| panic!("exact ID does not need a lookup")));
            assert!(!same_proof_recording(&playing, "0001", |_| panic!("invalid id")));
            for bad in [serde_json::json!("008"), serde_json::json!("https://other"), serde_json::json!(8), serde_json::Value::Null] {
                let mut invalid = response.clone();
                invalid[0]["proof_catalog_id"] = bad;
                assert!(lookup_known(canonical, canonical, metadata.clone(), |_| Ok(json_response(200, &invalid))).is_none());
            }
            assert_eq!(make(old, "a-private-recording").catalog_id(), old);
        }
    }

    #[test]
    fn older_service_recovers_an_exact_queue_id_receipt_after_a_canonical_miss() {
        let metadata = serde_json::json!({"title":"song","artists":["Artist"],"duration_ms":120000});
        let response = serde_json::json!([{"source":"Yandex Music","ticket":"a".repeat(32),
            "sha256":"a".repeat(64),"pair_sha256":"b".repeat(64)}]);
        let mut attempts = Vec::new();
        let hit = lookup_known("148716915", "57878398", metadata, |body| {
            attempts.push(body.clone());
            Ok(if body.get("include_aliases").is_some() { json_response(400, &serde_json::Value::Null) }
                else if body["catalog_id"] == "57878398" { json_response(200, &response) }
                else { json_response(200, &serde_json::json!([])) })
        }).unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(hit.proof_id(), Some("57878398"));
    }

    #[test]
    fn catalog_ticket_requires_a_distinct_public_id_and_exact_encoded_bytes_from_yandex() {
        let bytes = b"catalog audio bytes";
        let media = media_digest(bytes.as_slice()).unwrap();
        let load = |id: &str| {
            assert_eq!(id, "8");
            let response = tungstenite::http::Response::builder().header("content-length", bytes.len()).body(bytes.to_vec()).unwrap();
            Some((crate::stream::Buf::from_media(response.into(), &|| true).unwrap(), "MP3 320".into()))
        };
        let ticket = |body| json_response(200, &body);
        let good = serde_json::json!({"alternative_id":"8"});
        assert!(catalog_ticket_audio(ticket(good.clone()), "7", &media, &|| true, load).is_some());
        assert!(catalog_ticket_audio(ticket(good.clone()), "7", &"b".repeat(64), &|| true, load).is_none());
        assert!(catalog_ticket_audio(ticket(good.clone()), "7", &media, &|| false, |_| panic!("stale owner")).is_none());
        for status in [302, 403, 404] {
            assert!(catalog_ticket_audio(json_response(status, &good), "7", &media, &|| true, |_| panic!("invalid ticket")).is_none());
        }
        for bad in [serde_json::json!({"alternative_id":"7"}), serde_json::json!({"alternative_id":"08"}),
            serde_json::json!({"alternative_id":"ugc-private"}), serde_json::json!({"alternative_id":"https://example.org/a"}),
            serde_json::json!({"alternative_id":"8","url":"https://example.org/a"})] {
            assert!(catalog_ticket_audio(ticket(bad), "7", &media, &|| true, |_| panic!("untrusted address")).is_none());
        }
    }

    #[test]
    fn catalog_confirmation_exposes_only_public_ids_metadata_media_hash_and_pair_commitment() {
        let mut t = crate::api::parse_track(&serde_json::json!({"id":7,"title":" Song  ","durationMs":1000,"artists":[{"id":1,"name":"A"}]})).unwrap();
        let (baseline, media) = ("a".repeat(64), "b".repeat(64));
        let proof = pair_digest("7", &baseline, &media);
        assert_eq!(catalog_confirmation(&t, "8", &baseline, &media).unwrap(),
            serde_json::json!({"catalog_id":"7","alternative_id":"8","title":"song","artists":["A"],"duration_ms":1000,"sha256":media,"pair_sha256":proof}));
        assert!(catalog_confirmation(&t, "7", &baseline, &media).is_none());
        assert!(catalog_confirmation(&t, "private-8", &baseline, &media).is_none());
        assert!(catalog_confirmation(&t, "8", &media, &media).is_none());
        t.user_upload = true;
        assert!(catalog_confirmation(&t, "8", &baseline, &media).is_none());
    }

    #[test]
    fn featured_version_matches_only_the_entire_already_credited_artist() {
        let make = |id, version, explicit| crate::api::parse_track(&serde_json::json!({
            "id":id, "title":"ДНК", "durationMs":204000, "version":version, "explicit":explicit,
            "artists":[{"id":1,"name":"Джиган"},{"id":2,"name":"Артём Качер"}]
        })).unwrap();
        let clean = make(7, "", false);
        for label in ["feat. Артем Качер", "  FEATURING  Артём   КАЧЕР "] {
            let explicit = make(8, label, true);
            assert_eq!(public_metadata(&explicit).unwrap()["title"], "днк");
            assert_eq!(catalog_candidates(&clean, &[explicit]), ["8"]);
        }
        for label in ["feat. Другой", "feat. Артем", "feat. Артем Качер Remix", "Remix", "Live", "featuring"] {
            let other = make(8, label, true);
            assert_ne!(public_metadata(&other).unwrap()["title"], "днк");
            assert!(catalog_candidates(&clean, &[other]).is_empty());
        }
    }

    #[test]
    fn catalog_matches_editions_with_separately_listed_confirmed_guests() {
        let make = |id, version, guest_id| crate::api::parse_track(&serde_json::json!({
            "id":id, "title":"Song", "durationMs":225500, "version":version, "explicit":true,
            "artists":[{"id":1,"name":"Lead"},{"id":guest_id,"name":"Guest"}]
        })).unwrap();
        for label in ["Remix; feat. Guest", "featuring Guest; Remix", "Remix ; FEAT. Guest"] {
            let original = make(7, label, 2);
            assert_eq!(catalog_candidates(&original, &[make(8, "Remix", 2)]), ["8"]);
            assert_eq!(catalog_candidates(&make(7, "Remix", 2), &[make(8, label, 2)]), ["8"]);
            for other in ["", "Live", "Remix; Live", "Remix; feat. Stranger", "Remix; feat. Guest Live"] {
                assert!(catalog_candidates(&original, &[make(8, other, 2)]).is_empty());
            }
            assert!(catalog_candidates(&original, &[make(8, "Remix", 3)]).is_empty());
        }
        // Catalog normalization must not change existing service/cache metadata.
        let original = make(7, "Remix; feat. Guest", 2);
        assert_eq!(public_metadata(&original).unwrap()["title"], "song (remix; feat. guest)");
    }

    #[test]
    fn known_delivery_retries_transient_errors_once_with_a_fresh_ticket_for_the_same_proof() {
        let digest = media_digest(b"verified".as_slice()).unwrap();
        let make = |ticket: &str, pair: &str| serde_json::from_value::<KnownCandidate>(serde_json::json!({
            "source":"Bandcamp","ticket":ticket.repeat(32),"sha256":digest,"pair_sha256":pair.repeat(64),"proof_catalog_id":"7"
        })).unwrap();
        let old = make("1", "a");
        let body = |status, declared, data: &[u8]| tungstenite::http::Response::builder().status(status)
            .header("content-length", declared).body(data.to_vec()).unwrap().into();
        // A Vec-backed reqwest fixture reports its actual byte length. Use HTTP to
        // reproduce a connection that closes before the declared body is delivered.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut input = std::io::BufReader::new(&stream);
            let mut line = String::new();
            loop {
                line.clear();
                if input.read_line(&mut line).unwrap() == 0 || line == "\r\n" { break; }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncut").unwrap();
            stream.flush().unwrap();
        });
        let partial = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(2))
            .build().unwrap().get(endpoint).send().unwrap();
        peer.join().unwrap();
        for first in [body(502,0,b""), body(503,0,b""), body(504,0,b""), partial] {
            let mut first = Some(first);
            let mut issued = vec![];
            let buf = deliver_known(&old, &|| true, || Some(make("2", "a")), |ticket| {
                issued.push(ticket.ticket.clone());
                ticket_audio(first.take().unwrap_or_else(|| body(200,8,b"verified")), Some(&digest), None, &|| true)
            }).unwrap();
            assert_eq!(issued, ["1".repeat(32), "2".repeat(32)]);
            assert_eq!(media_digest(buf.reader()).unwrap(), digest);
        }
        for refusal in [body(401,0,b""), body(403,0,b""), body(404,0,b""), body(500,0,b""), body(304,0,b""), body(200,8,b"tampered")] {
            let mut response = Some(refusal);
            assert!(deliver_known(&old, &|| true, || panic!("terminal refusal"), |_| {
                ticket_audio(response.take().unwrap(), Some(&digest), None, &|| true)
            }).is_none());
        }
        let active = std::cell::Cell::new(true);
        assert!(deliver_known(&old, &|| active.get(), || panic!("cancelled owner"), |_| {
            active.set(false);
            Err::<(),_>(AudioError::Transport)
        }).is_none());
        for replacement in [make("1", "a"), make("2", "b")] {
            let mut attempts = 0;
            assert!(deliver_known(&old, &|| true, || Some(replacement), |_| {
                attempts += 1;
                Err::<(),_>(AudioError::Busy)
            }).is_none());
            assert_eq!(attempts, 1);
        }
        let mut attempts = 0;
        assert!(deliver_known(&old, &|| true, || Some(make("2", "a")), |_| {
            attempts += 1;
            Err::<(),_>(AudioError::Busy)
        }).is_none());
        assert_eq!(attempts, 2);
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
        assert!(ticket_audio(response(304, b""), Some(&hash), Some(buf.clone()), &|| true).is_err());
        let tagged = |digest: &str| tungstenite::http::Response::builder().status(304)
            .header("etag",format!(r#""{digest}""#)).body(Vec::<u8>::new()).unwrap().into();
        assert!(ticket_audio(tagged(&"a".repeat(64)), Some(&hash), Some(buf.clone()), &|| true).is_err());
        let cached = ticket_audio(tagged(&hash), Some(&hash), Some(buf.clone()), &|| true).unwrap();
        assert!(std::sync::Arc::ptr_eq(&buf, &cached));
    }

    #[test]
    fn personal_replay_marker_requires_its_session_and_exact_pair() {
        let baseline = "b".repeat(64);
        let media = "a".repeat(64);
        let pair = pair_digest("123", &baseline, &media);
        let hit: KnownCandidate = serde_json::from_value(serde_json::json!({
            "source":"Bandcamp","ticket":"1".repeat(32),"sha256":media,"pair_sha256":pair,
            "proof_catalog_id":"123","acoustic_review_version":3
        })).unwrap();
        let token = format!("fixture-{}-{}", std::process::id(), pair);
        hit.remember_local_review("123", &baseline, &token);
        assert!(hit.has_local_review("123", &token));
        assert!(hit.locally_reviewed("123", &baseline, &token));
        assert!(!hit.has_local_review("123", "another account"));
        assert!(!hit.locally_reviewed("123", &"c".repeat(64), &token));
        assert!(!hit.locally_reviewed("124", &baseline, &token));
        assert!(!hit.reviewed("123", &baseline), "local replay is not a shared receipt");
        let _ = std::fs::remove_file(local_review_path(&token, &pair));
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
        let mixed = make(5, "Latin title");
        assert!(should_uncensor(&api, &mixed, || Ok(Some(true)), || None));
        assert!(!should_uncensor(&api, &make(6, "Foreign vocals"), || panic!("known vocal language"), || Some(false)));
        let unknown = make(4, "Instrumental");
        assert!(!should_uncensor(&api, &unknown, || Ok(None), || None));
    }

    #[test]
    fn local_review_is_bound_to_track_and_both_files_and_does_not_accept_a_bucket_collision() {
        let path = std::env::temp_dir().join(format!("ym-local-review-test-{}",std::process::id()));
        let pair = pair_digest("7", &"a".repeat(64), &"b".repeat(64));
        write_local_review(&path, &pair).unwrap();
        assert!(read_local_review(&path, &pair));
        for other in [pair_digest("8", &"a".repeat(64), &"b".repeat(64)),
            pair_digest("7", &"c".repeat(64), &"b".repeat(64)),
            pair_digest("7", &"a".repeat(64), &"c".repeat(64))] {
            assert!(!read_local_review(&path,&other));
        }
        std::fs::write(&path,b"explicit_badge").unwrap();
        assert!(!read_local_review(&path,&pair));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_metadata_cannot_establish_a_verified_pair() {
        for edition in ["explicit_badge", "explicit_version", "uncensored"] {
            let row = serde_json::json!({"source":"YouTube Music", "ticket":"a".repeat(32),
                "edition":edition, "sha256":"b".repeat(64)});
            let candidate: OnlineCandidate = serde_json::from_value(row.clone()).unwrap();
            assert!(candidate.valid()); // Searchable, but not a proof of restoration.
            let known: KnownCandidate = serde_json::from_value(row).unwrap();
            assert!(!known.has_fast_proof());
            assert!(!known.valid("7"));
        }
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
        assert_eq!(catalog_candidates(&unknown, &hits), ["5"]);
    }
}
