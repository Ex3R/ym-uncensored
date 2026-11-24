//! Аудио-движок: очередь, загрузка и предзагрузка треков, фидбек волне, воспроизведение.
//! Живёт в своём потоке, UI шлёт ему команды и читает `Shared`.

use crate::ahead::Ahead;
use crate::api::{Account, Api, Batch, Res, Track, WaveSession, WAVE};
use crate::config::log;
use crate::eq::{Eq, EqParams};
use crate::fade::{Fade, FadeOut};
use crate::stream::{Buf, Reader};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source as _};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Default, Debug)]
pub enum Status {
    #[default]
    LoggedOut,
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListKind {
    Playlist,
    Album,
    Artist,
    Likes,
    Tracks,
}

/// Что сейчас играет.
#[derive(Clone, PartialEq, Debug, Default)]
pub enum Source {
    #[default]
    Wave,
    /// Волна по станции: `artist:ID`, `track:ID`, `album:ID`, `playlist:UID_KIND`.
    Seed { station: String, header: String, title: String },
    List { kind: ListKind, id: String, title: String },
}

impl Source {
    pub fn station(&self) -> Option<&str> {
        match self {
            Source::Wave => Some(WAVE),
            Source::Seed { station, .. } => Some(station),
            Source::List { .. } => None,
        }
    }
}

/// Что играть, когда список закончится.
#[derive(Clone, Debug)]
pub enum After {
    Loop,
    Wave(Source),
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Default)]
pub struct Shared {
    pub status: Status,
    pub uid: String,
    pub login: String,
    pub has_plus: bool,
    pub track: Option<Track>,
    pub source: Source,
    pub upcoming: Vec<Track>,
    pub playing: bool,
    pub likes_revision: i32,
    pub loading: bool,
    pub waiting_for_playback: bool,
    pub position: f32,
    track_generation: u64,
    seek_request: u64,
    pub duration: f32,
    pub volume: f32,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub error: Option<(String, Instant)>,
    /// Трек продолжен с того места, где его оставили в прошлый раз: плеер виден сразу, на паузе.
    pub resumed: bool,
    /// Формат играющего файла: «MP3 320», «AAC».
    pub format: String,
}

/// Как положение ползунка громкости переводится в усиление.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Loudness {
    Square,
    /// 50 дБ на всю шкалу: каждый шаг ползунка на слух одинаковый (#44).
    Decibels,
}

pub enum Cmd {
    Login(String),
    Logout,
    PlayPause,
    /// Снять с паузы (если уже играет — ничего не делать).
    Resume,
    Next,
    Prev,
    Seek(f32),
    Volume(f32),
    Loudness(Loudness),
    LikeTrack(Track, Option<LikeCompleted>),
    Dislike,
    Wave(Source),
    /// Заново начать «Мою волну» с настройками (seeds `setting…`).
    TuneWave(Vec<String>),
    PlayList { source: Source, tracks: Vec<Track>, start: usize, after: After },
    /// Перейти к треку из очереди (индекс в `upcoming`).
    Jump(usize),
    Repeat(Repeat),
    Shuffle(bool),
    /// «Играть следующим» (`next`) или «Добавить в конец очереди».
    Enqueue { track: Track, next: bool },
    /// Переключатель «Треки 18+».
    AllowExplicit(bool),
    SourcesModeChanged,
    /// Сколько треки перетекают друг в друга; ноль — без кроссфейда.
    Crossfade(Duration),
    /// Приложение закрывается: записать в Ynison, где остановились, и ответить.
    Leave(Sender<()>),
}

enum Msg {
    Cmd(Cmd),
    Seek { seconds: f32, track_generation: u64, request: u64 },
    Account { gen: u64, res: Res<Account> },
    Likes(String, Res<Vec<String>>),
    LikeFinished { uid: String, id: String, liked: bool, result: Res<()> },
    Batch { res: Res<Batch>, replace: bool, gen: u64 },
    Audio { gen: u64, res: Res<LoadedTrack> },
    Prefetch { gen: u64, id: String, res: Res<LoadedTrack> },
    KnownFinished { owner: std::sync::Weak<()>, revision: u64, audio: Option<Audio> },
    Replacement { job: std::sync::Weak<()>, audio: Option<Audio> },
    Resume { gen: u64, res: Res<Option<Resume>> },
    Error(String),
}

type Notify = Arc<dyn Fn() + Send + Sync>;
type LikeCompleted = Box<dyn FnOnce(&Track, bool, &Res<()>) + Send>;
type Job = Box<dyn FnOnce() + Send>;
pub type Likes = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
pub struct Player {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    pub eq: Arc<EqParams>,
}

impl Player {
    pub fn spawn(notify: Notify) -> Player {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared { volume: 0.6, ..Default::default() }));
        let likes: Likes = Default::default();
        let eq: Arc<EqParams> = Default::default();
        let p = Player { tx: tx.clone(), shared: shared.clone(), likes: likes.clone(), eq: eq.clone() };
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || Engine::new(tx, shared, likes, notify, eq).run(rx))
            .expect("audio thread");
        p
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(Msg::Cmd(c));
    }

    /// Перед выходом: дождаться, пока Ynison запишет, где остановились, но не дольше 3 секунд.
    pub fn leave(&self) {
        let (done, wait) = mpsc::channel();
        self.send(Cmd::Leave(done));
        let _ = wait.recv_timeout(Duration::from_secs(3));
    }

    /// Сразу запоминаем выбранное время, иначе бегунок на миг вернётся к старой позиции.
    pub fn seek(&self, seconds: f32) -> Option<f32> {
        let mut s = self.shared.lock().unwrap();
        if !seconds.is_finite() || s.loading || s.track.is_none() || s.duration <= 0.0 {
            return None;
        }
        let seconds = seconds.clamp(0.0, s.duration);
        let request = s.seek_request.wrapping_add(1);
        self.tx.send(Msg::Seek { seconds, track_generation: s.track_generation, request }).ok()?;
        s.seek_request = request;
        s.position = seconds;
        Some(seconds)
    }

    pub fn state(&self) -> Shared {
        self.shared.lock().unwrap().clone()
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.likes.lock().unwrap().contains(id)
    }

}

/// Сколько живёт сессия волны без прослушивания: оригинал начинает новую через 9 часов.
const WAVE_SESSION_TTL: u64 = 9 * 3600;

/// `from` «Моей волны» у оригинала, когда её запускают с главной.
const WAVE_FROM: &str = "desktop-wave_landing_screen-my_wave-radio-default";

struct WaveState {
    uid: String,
    session: String,
    /// Последний трек волны: с ним сессия клонируется после перезапуска.
    last: String,
    at: u64,
}

fn load_wave_state() -> Option<WaveState> {
    let text = std::fs::read_to_string(crate::config::dir().join("wave.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let str_at = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Some(WaveState {
        uid: str_at("uid"),
        session: str_at("session"),
        last: str_at("last"),
        at: v.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// Где звучит трек — для `/plays`, в тех же полях, что у оригинала.
#[derive(Clone)]
struct PlayContext {
    from: String,
    context: &'static str,
    item: Option<String>,
    playlist: Option<String>,
    /// Сессия волны; пустая у списков.
    session: String,
}

/// Одно прослушивание трека: сколько его реально слушали, была ли пауза и перемотка.
/// Уходит в `/plays`, когда трек сменился.
struct Listen {
    track: Track,
    ctx: PlayContext,
    play_id: String,
    add_time: String,
    start: String,
    played: f64,
    since: Option<Instant>,
    pause: bool,
    seek: bool,
    repeated: bool,
}

impl Listen {
    fn played(&self) -> f64 {
        self.played + self.since.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    fn stop(&mut self) {
        if let Some(s) = self.since.take() {
            self.played += s.elapsed().as_secs_f64();
        }
    }

    fn go(&mut self) {
        self.since.get_or_insert_with(Instant::now);
    }
}

/// Случайные 128 бит без отдельной зависимости: sha256 от времени, счётчика и pid.
fn random_u128() -> u128 {
    use sha2::{Digest, Sha256};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let h = Sha256::digest(format!("{t}:{n}:{}", std::process::id()));
    u128::from_le_bytes(h[..16].try_into().unwrap())
}

/// uuid v4, как `playId` у оригинала.
fn uuid4() -> String {
    let mut b = random_u128().to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Трек, который качается, и декодер, уже прочитавший его начало.
/// Скачиваемый трек, его декодер и формат («MP3 320»).
type Audio = (Arc<Buf>, Decoder<Reader>, String);

struct LoadedTrack {
    baseline: Audio,
    known: KnownStatus,
}

enum KnownStatus {
    Miss(u64),
    Pending(u64),
    Verified(u64, Audio),
}

impl KnownStatus {
    fn attempted_revision(&self) -> u64 {
        match self {
            Self::Miss(r) | Self::Pending(r) | Self::Verified(r, _) => *r,
        }
    }

    fn revision(&self) -> Option<u64> {
        match self {
            Self::Pending(revision) | Self::Verified(revision, _) => Some(*revision),
            Self::Miss(_) => None,
        }
    }
}

struct ReplacementCheck {
    id: String,
    revision: u64,
    owner: Arc<()>,
    finished: bool,
    audio: Option<Audio>,
}

impl ReplacementCheck {
    fn matches(&self, track: &Track, revision: u64) -> bool {
        self.id == track.id && self.revision == revision
    }
}

/// Имя выхода Windows по умолчанию. На Linux звук идёт в «default», и за сменой устройства
/// там следит PipeWire или PulseAudio.
#[cfg(windows)]
fn default_output_name() -> Option<String> {
    use rodio::cpal::traits::HostTrait;
    use rodio::DeviceTrait;
    rodio::cpal::default_host().default_output_device()?.name().ok()
}

/// Многоканальные выходы Windows (виртуальные 7.1 у гарнитур) искажают f32, а i32 играют чисто: cpal#892.
#[cfg(windows)]
fn open_surround() -> Option<(OutputStream, rodio::OutputStreamHandle)> {
    use rodio::cpal::traits::HostTrait;
    use rodio::cpal::SampleFormat;
    use rodio::DeviceTrait;
    let dev = rodio::cpal::default_host().default_output_device()?;
    let def = dev.default_output_config().ok()?;
    if def.channels() <= 2 || def.sample_format() == SampleFormat::I32 {
        return None;
    }
    let rate = def.sample_rate();
    let cfg = dev
        .supported_output_configs()
        .ok()?
        .find(|c| c.sample_format() == SampleFormat::I32 && c.channels() == def.channels() && c.min_sample_rate() <= rate && rate <= c.max_sample_rate())?
        .with_sample_rate(rate);
    let out = OutputStream::try_from_device_config(&dev, cfg).ok()?;
    log(format!("аудиовыход: {} каналов, i32", def.channels()));
    Some(out)
}

/// Только выход по умолчанию: `try_default` при неудаче молча берёт любое другое устройство,
/// и звук уходит, например, в неподключённый HDMI (#53).
fn open_output() -> Result<(OutputStream, rodio::OutputStreamHandle), rodio::StreamError> {
    use rodio::cpal::traits::HostTrait;
    #[cfg(windows)]
    if let Some(out) = open_surround() {
        return Ok(out);
    }
    let dev = rodio::cpal::default_host().default_output_device().ok_or(rodio::StreamError::NoDevice)?;
    OutputStream::try_from_device(&dev)
}

#[cfg(not(target_os = "linux"))]
const NO_OUTPUT: &str = "Не найдено аудиоустройство";
#[cfg(target_os = "linux")]
const NO_OUTPUT: &str = "Не открылся звуковой выход по умолчанию. Для PipeWire нужен pipewire-alsa";

/// Как часто записывать позицию играющего трека в Ynison.
const YNISON_EVERY: Duration = Duration::from_secs(60);

/// Сколько позиция может стоять на месте у играющего скачанного трека.
const STALL_LIMIT: Duration = Duration::from_secs(3);

fn replacements_enabled() -> bool {
    crate::api::allow_explicit() && crate::resolver_login::mode_enabled()
}

// Once the catalog decoder is ready, allow a cold TLS lookup, then a separate delivery
// window only for an actual match. Misses and errors wake the download worker immediately.
const LOOKUP_GRACE: Duration = Duration::from_secs(2);
const FIRST_BUFFER_WAIT: Duration = Duration::from_secs(7);

enum KnownEvent {
    Found,
    Verified(Audio),
}

fn initial_status(events: &Receiver<KnownEvent>, revision: u64, wait: Duration) -> KnownStatus {
    let mut timeout = wait.min(LOOKUP_GRACE);
    loop {
        match events.recv_timeout(timeout) {
            Ok(KnownEvent::Verified(audio)) => return KnownStatus::Verified(revision, audio),
            Ok(KnownEvent::Found) => timeout = wait,
            Err(RecvTimeoutError::Disconnected) => return KnownStatus::Miss(revision),
            Err(RecvTimeoutError::Timeout) => return KnownStatus::Pending(revision),
        }
    }
}

fn verified_known(
    api: &Api, track: &Track, current: &impl Fn() -> bool,
    found: impl FnOnce() -> bool, baseline: impl FnOnce() -> Option<Arc<Buf>>,
) -> Option<Audio> {
    use crate::originals;
    if !current() { return None; }
    if !originals::can_match(track) { return None; }
    let token = crate::resolver_login::credential()?;
    let hit = originals::known_candidate(track, &token, current)?;
    let proof = hit.proof_id()?;
    if !current() || !found() { return None; }
    if !originals::same_proof_recording(track, proof, |ids| api.tracks(ids)) || !current() { return None; }
    let media = originals::known_audio(&hit, proof, token, current, |alternative| {
        let public = api.tracks(&[alternative.into()]).ok()?.into_iter()
            .any(|t| t.id == alternative && t.catalog_id() != track.catalog_id() && originals::can_match(&t));
        if !public || !current() { return None; }
        load_media(api, alternative).ok()
    })?;
    let digest = originals::complete_digest(&baseline()?, current)?;
    if !hit.matches(proof, &digest) { return None; }
    let decoder = open_decoder(&media.buf).ok()?;
    if !current() { return None; }
    let audio = (media.buf.clone(), decoder, media.format.clone());
    originals::cache_known(&hit, proof, &media);
    Some(audio)
}

fn load_for_playback(
    api: Api, track: Track, owner: std::sync::Weak<()>, tx: Sender<Msg>,
    deliver: impl FnOnce(Res<LoadedTrack>),
) {
    let revision = crate::resolver_login::session_revision();
    let (give_baseline, baseline) = mpsc::channel::<Arc<Buf>>();
    let (result, known) = mpsc::channel::<KnownEvent>();
    if replacements_enabled() && crate::originals::can_match(&track) {
        let catalog = api.clone();
        let requested = track.clone();
        let job = owner.clone();
        thread::spawn(move || {
            let current = || job.strong_count() != 0 && replacements_enabled()
                && crate::resolver_login::session_revision() == revision;
            let found = || result.send(KnownEvent::Found).is_ok();
            if let Some(audio) = verified_known(&catalog, &requested, &current, found, || baseline.recv().ok()) {
                let _ = result.send(KnownEvent::Verified(audio));
            }
        });
    } else {
        drop(result);
    }
    let baseline = match load_audio(&api, &track.id) {
        Ok(audio) => audio,
        Err(error) => return deliver(Err(error)),
    };
    let _ = give_baseline.send(baseline.0.clone());
    let status = initial_status(&known, revision, FIRST_BUFFER_WAIT);
    let pending = matches!(status, KnownStatus::Pending(_));
    deliver(Ok(LoadedTrack { baseline, known: status }));
    if pending {
        let audio = known.into_iter().find_map(|event| match event {
            KnownEvent::Verified(audio) => Some(audio), KnownEvent::Found => None,
        });
        let _ = tx.send(Msg::KnownFinished { owner, revision, audio });
    }
}

fn resolve_track(
    api: &Api, t: &Track, buf: Arc<Buf>, current: &impl Fn() -> bool, retry: &std::cell::Cell<bool>,
) -> Option<Audio> {
    use crate::{originals, verification::Recording};
    let revision = crate::resolver_login::session_revision();
    let lyrics = std::cell::OnceCell::new();
    let reference_text = || lyrics.get_or_init(|| {
        let result = api.reference_lyrics(t);
        if let Ok(Some(_)) = &result { return result; }
        if t.lyrics {
            let deadline = Instant::now() + Duration::from_secs(3);
            match api.lyrics_until(&t.id, Some(deadline)) {
                Ok(found) if !found.text.is_empty() || !found.synced.is_empty() => return Ok(Some(found)),
                Err(e) => return Err(e),
                _ => {}
            }
        }
        result
    }).as_ref().map_err(|e| { retry.set(true); e.clone() });
    let eligible = current() && replacements_enabled() && originals::should_uncensor(api, t,
        || reference_text().map(|l| l.as_ref().and_then(crate::language::lyrics_language)),
        || crate::language::vocal_language(open_decoder(&buf).ok()?, t.duration_ms));
    if eligible {
        log(format!("Замена {}: подбор", t.id));
        let baseline = std::cell::OnceCell::new();
        let check = |candidate: Audio| -> Option<Audio> {
            if !current() { return None; }
            let reference = baseline.get_or_init(|| Recording::decode(open_decoder(&buf).ok()?, t.duration_ms)).as_ref().or_else(|| { log("Замена: исходное аудио не декодировано"); None })?;
            let alternative = Recording::decode(open_decoder(&candidate.0).ok()?, t.duration_ms)
                .or_else(|| { log("Замена: не удалось прочитать альтернативную запись"); None })?;
            if reference.digest == alternative.digest { return None; }
            let lyrics = reference_text().ok()?.as_ref().or_else(|| { log("Замена: отсутствует текст для проверки"); None })?;
            if !current() { return None; }
            let time = originals::verified(api, reference, &alternative, lyrics)
                .or_else(|| { log("Замена: изменения вокала не подтверждены"); None })?;
            log(format!("Подтверждена замена {} на {:.1} с", t.id, time));
            Some(candidate)
        };
        let mut seen = std::collections::HashSet::new();
        let mut try_yandex = |id: &str, label: &str| {
            if !current() || !seen.insert(id.to_owned()) { return None; }
            let (buf, decoder, format) = load_audio(api, id).ok()?;
            check((buf, decoder, format!("{format} · {label}")))
        };
        for id in originals::catalog_versions(api, t) {
            if let Some(audio) = try_yandex(&id, "Без цензуры") {
                if current() { originals::remember_catalog(t, id, buf.clone(), audio.0.clone(), revision); }
                return Some(audio);
            }
        }
        if !current() { return None; }
        let candidates = originals::online_candidates(t).inspect_err(|_| { retry.set(true); log("Замена: внешний поиск недоступен"); }).unwrap_or_default();
        log(format!("Замена {}: внешних кандидатов {}", t.id, candidates.len()));
        let next = candidates.first().and_then(|candidate| candidate.next.clone());
        let original_digest = std::cell::OnceCell::new();
        let original_digest = || original_digest.get_or_init(|| originals::media_digest(buf.reader()).ok()).as_deref();
        let try_external = |candidates: Vec<originals::OnlineCandidate>| {
            for candidate in candidates {
                if !current() { return None; }
                if let Ok(result) = originals::online_audio(&candidate,t,original_digest(),current)
                    .inspect_err(|_| { retry.set(true); log("Замена: ошибка загрузки аудио"); }) {
                    if let Ok(decoder) = open_decoder(&result.buf) {
                        let audio = (result.buf.clone(),decoder,result.format.clone());
                        if !current() { return None; }
                        if result.cached {
                            log(format!("Замена {}: подтверждённое аудио из кэша",t.id));
                            // Migrate a previously verified, baseline-bound v0.2.2 cache hit.
                            originals::remember_external(result, &candidate, t.catalog_id(), original_digest());
                            return Some(audio);
                        }
                        if let Some(audio) = check(audio) {
                            originals::remember_external(result,&candidate,t.catalog_id(),original_digest());
                            return Some(audio);
                        }
                    }
                }
            }
            None
        };
        if let Some(audio) = try_external(candidates) { return Some(audio); }
        if !current() { return None; }
        if let Some(next) = next {
            if let Some(audio) = try_external(originals::online_next(&next).inspect_err(|_| retry.set(true)).unwrap_or_default()) { return Some(audio); }
        }
    }
    None
}

fn load_media(api: &Api, track_id: &str) -> Res<(Arc<Buf>, String)> {
    let (url, format) = api.stream_url(track_id)?;
    let (again, id) = (api.clone(), track_id.to_owned());
    // Ссылка подписана и живёт недолго, поэтому для докачки берётся новая.
    let buf = Buf::download(api.open_from(&url, 0)?, move |from| again.open_from(&again.stream_url(&id)?.0, from))?;
    Ok((buf, format))
}

fn load_audio(api: &Api, track_id: &str) -> Res<Audio> {
    let (buf, format) = load_media(api, track_id)?;
    let dec = open_decoder(&buf)?;
    Ok((buf, dec, format))
}

fn open_decoder(buf: &Arc<Buf>) -> Res<Decoder<Reader>> {
    // Rodio 0.20 panics if a demuxer reports a seek error while probing a stream.
    std::panic::catch_unwind(|| Decoder::new(buf.reader()))
        .map_err(|_| "декодер не смог открыть аудиофайл".to_string())?
        .map_err(|e| format!("не удалось декодировать: {e}"))
}

/// Что последним играло в любом клиенте, уже с треками из API.
struct Resume {
    source: Source,
    tracks: Vec<Track>,
    index: usize,
    position: f32,
}

fn fetch_resume(api: &Api) -> Res<Option<Resume>> {
    let p = crate::ynison::playing(api.token(), &crate::ynison::device_id())?;
    let Some(current) = p.track_id() else { return Ok(None) };
    // Волна дальше подберёт треки сама, из её очереди нужен только текущий.
    let ids = if p.entity_type == "RADIO" { vec![current.to_string()] } else { p.track_ids.clone() };
    let tracks = api.tracks(&ids)?;
    let Some(index) = tracks.iter().position(|t| t.id == current) else { return Ok(None) };
    let track = &tracks[index];
    let artist = |id: &str| track.artist_list.iter().find(|a| a.id == id).map(|a| a.name.clone()).unwrap_or_default();
    let album = |id: &str| if track.album_id.as_deref() == Some(id) { track.album_title.clone() } else { String::new() };
    let entity = p.entity_id.clone();
    let source = match p.entity_type.as_str() {
        "RADIO" if entity == WAVE => Source::Wave,
        "RADIO" => {
            let (kind, id) = entity.split_once(':').unwrap_or((entity.as_str(), ""));
            let (header, title) = match kind {
                "artist" => ("Моя волна по исполнителю", artist(id)),
                "album" => ("Моя волна по альбому", album(id)),
                "track" if id == track.id => ("Моя волна по треку", track.title.clone()),
                "track" => ("Моя волна по треку", String::new()),
                "playlist" => ("Моя волна по плейлисту", String::new()),
                _ => ("Моя волна", String::new()),
            };
            Source::Seed { station: entity.clone(), header: header.into(), title }
        }
        "PLAYLIST" => Source::List { kind: ListKind::Playlist, title: String::new(), id: entity },
        "ALBUM" => Source::List { kind: ListKind::Album, title: album(&entity), id: entity },
        "ARTIST" => Source::List { kind: ListKind::Artist, title: artist(&entity), id: entity },
        _ => Source::List { kind: ListKind::Tracks, id: String::new(), title: String::new() },
    };
    Ok(Some(Resume { source, tracks, index, position: p.progress_ms as f32 / 1000.0 }))
}

/// Перемешивание без лишних зависимостей (xorshift от времени).
pub fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

enum End {
    Finished,
    Skip,
    Dislike,
    /// Волну запустили заново: пропуск, но очередь уже свежая.
    Restart,
}

struct ListPlay {
    deferred: VecDeque<Track>,
    all: Vec<Track>,
    after: After,
}

struct Engine {
    tx: Sender<Msg>,
    bg: Sender<Job>,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    // Запоминаем и снятые лайки, иначе старый список с сервера может их вернуть.
    like_writes: HashMap<String, LikeWrite>,
    notify: Notify,
    eq: Arc<EqParams>,
    audio: Option<(OutputStream, OutputStreamHandle, Sink)>,
    /// Затухание играющего трека.
    fade_out: Arc<FadeOut>,
    /// Прошлый трек, который затухает под начало нового.
    fading: Option<Sink>,
    crossfade: Duration,
    /// С каким нарастанием начать следующий загруженный трек.
    fade_in: Duration,
    /// Выход Windows по умолчанию, на котором открыт звук, и когда его последний раз сверяли.
    #[cfg(windows)]
    out_name: Option<String>,
    #[cfg(windows)]
    out_checked: Instant,
    /// Выход пропал посреди трека: с этого места трек продолжится по Play.
    held_pos: Option<f32>,
    /// Позиция играющего трека и с какого момента она не меняется.
    stall: (f32, Instant),
    api: Option<Api>,
    login_gen: u64,
    uid: String,
    source: Source,
    list: Option<ListPlay>,
    queue: VecDeque<Track>,
    history: Vec<Track>,
    current: Option<Track>,
    pending_replacement: Arc<Mutex<Option<Job>>>,
    replacement_wakeup: mpsc::SyncSender<()>,
    current_check: Option<ReplacementCheck>,
    current_lookup_revision: Option<u64>,
    next_check: Option<ReplacementCheck>,
    current_buf: Option<Arc<Buf>>,
    // The catalog-id baseline survives an audio-only replacement and a sources-session change.
    catalog_buf: Option<(Arc<Buf>, String)>,
    batch_id: String,
    /// Сессия ротора текущей волны; radioStarted уходит в неё, когда она появится.
    session: String,
    radio_started_pending: bool,
    /// Пропуски и дослушанные треки, которые уйдут со следующим запросом треков волны.
    deferred: Vec<serde_json::Value>,
    /// Станция, для которой уже проверена сохранённая сессия «Моей волны».
    played_station: String,
    /// Сессия «Моей волны» с прошлого запуска уже подхвачена (или не нашлась).
    wave_restored: bool,
    /// Сессия с прошлого запуска: первый пакет берётся её клоном, как у оригинала.
    clone_pending: bool,
    /// Последний трек волны с прошлого запуска — queue для клона.
    restore_last: Option<String>,
    /// Настройки, с которыми начать следующую сессию «Моей волны».
    tune: Option<Vec<String>>,
    /// С какой секунды продолжить трек, когда он загрузится.
    resume_at: Option<(String, f32)>,
    /// Когда записать состояние в Ynison: правки идут пачкой (перемотка, пауза), пишем одну.
    ynison_due: Option<Instant>,
    ynison_at: Instant,
    /// Текущее прослушивание для `/plays`.
    listen: Option<Listen>,
    fetching: bool,
    batch_gen: u64,
    retry_at: Option<Instant>,
    gen: u64,
    loaded: bool,
    paused: bool,
    volume: f32,
    loudness: Loudness,
    repeat: Repeat,
    shuffle: bool,
    prefetch: Option<(String, LoadedTrack)>,
    current_load: Option<Arc<()>>,
    paused_start: Option<LoadedTrack>,
    prefetch_load: Option<Arc<()>>,
    load_started: Instant,
    prefetching: Option<String>,
    prefetch_failure: Option<(String, Instant)>,
    prefetch_gen: u64,
    errors_in_row: u32,
    was_playing: bool,
    handled_seek: u64,
}

struct LikeWrite {
    pending: usize,
    saved: bool,
}

impl Engine {
    fn new(tx: Sender<Msg>, shared: Arc<Mutex<Shared>>, likes: Likes, notify: Notify, eq: Arc<EqParams>) -> Self {
        // Фидбек и лайки — по порядку в отдельном потоке, чтобы не блокировать плеер.
        let (bg, bg_rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("bg".into())
            .spawn(move || {
                for job in bg_rx {
                    job();
                }
            })
            .expect("bg thread");
        // Keep only the most recent pending track while a comparison is in progress.
        let pending_replacement = Arc::new(Mutex::new(None::<Job>));
        let pending = pending_replacement.clone();
        let (replacement_wakeup, wakeup) = mpsc::sync_channel::<()>(1);
        thread::spawn(move || {
            while wakeup.recv().is_ok() {
                let job = pending.lock().unwrap().take();
                if let Some(job) = job { job(); }
            }
        });
        Engine {
            tx,
            bg,
            shared,
            likes,
            like_writes: HashMap::new(),
            notify,
            eq,
            audio: None,
            #[cfg(windows)]
            out_name: None,
            #[cfg(windows)]
            out_checked: Instant::now(),
            held_pos: None,
            stall: (0.0, Instant::now()),
            api: None,
            login_gen: 0,
            uid: String::new(),
            source: Source::Wave,
            list: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            current: None,
            current_buf: None,
            catalog_buf: None,
            pending_replacement,
            replacement_wakeup,
            current_check: None,
            current_lookup_revision: None,
            next_check: None,
            batch_id: String::new(),
            session: String::new(),
            radio_started_pending: false,
            deferred: Vec::new(),
            played_station: String::new(),
            wave_restored: false,
            clone_pending: false,
            tune: None,
            resume_at: None,
            ynison_due: None,
            ynison_at: Instant::now(),
            restore_last: None,
            listen: None,
            fetching: false,
            batch_gen: 0,
            retry_at: None,
            gen: 0,
            loaded: false,
            paused: true,
            volume: 0.6,
            loudness: Loudness::Square,
            repeat: Repeat::Off,
            shuffle: false,
            prefetch: None,
            current_load: None,
            paused_start: None,
            prefetch_load: None,
            load_started: Instant::now(),
            prefetching: None,
            prefetch_failure: None,
            prefetch_gen: 0,
            errors_in_row: 0,
            was_playing: false,
            handled_seek: 0,
            fade_out: Arc::default(),
            fading: None,
            crossfade: Duration::ZERO,
            fade_in: Duration::ZERO,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(msg) => {
                    self.handle(msg);
                    self.sync();
                    (self.notify)();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    // ---------- вспомогательное ----------

    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn sink(&self) -> Option<&Sink> {
        self.audio.as_ref().map(|(_, _, s)| s)
    }

    fn ensure_audio(&mut self) -> bool {
        if self.audio.is_none() {
            match open_output() {
                Ok((stream, handle)) => match Sink::try_new(&handle) {
                    Ok(sink) => {
                        self.audio = Some((stream, handle, sink));
                        #[cfg(windows)]
                        {
                            self.out_name = default_output_name();
                        }
                    }
                    Err(e) => log(format!("sink: {e}")),
                },
                Err(e) => log(format!("audio device: {e}")),
            }
        }
        self.audio.is_some()
    }

    fn gain(&self) -> f32 {
        let v = self.volume;
        match self.loudness {
            Loudness::Square => v * v,
            Loudness::Decibels if v <= 0.0 => 0.0,
            Loudness::Decibels => 10f32.powf((v - 1.0) * 50.0 / 20.0),
        }
    }

    fn apply_volume(&self) {
        let gain = self.gain();
        for s in self.sink().into_iter().chain(&self.fading) {
            s.set_volume(gain);
        }
    }

    /// Источник для звука: эквалайзер, декодирование наперёд и затухание для кроссфейда.
    fn source(&mut self, src: Decoder<Reader>, fade_in: Duration) -> impl rodio::Source<Item = f32> + Send + 'static {
        self.fade_out = Arc::default();
        Fade::new(Ahead::new(Eq::new(src.convert_samples::<f32>(), self.eq.clone())), fade_in, self.fade_out.clone())
    }

    /// Пора ли начать кроссфейд: до конца трека осталось его время и следующий трек уже скачан.
    fn crossfade_due(&self) -> bool {
        if self.crossfade.is_zero() || !self.loaded || self.paused || self.fading.is_some() || self.repeat == Repeat::One {
            return false;
        }
        let next_ready = match (self.queue.front(), &self.prefetch) {
            (Some(next), Some((id, _))) => *id == next.id,
            _ => false,
        };
        let fade = self.crossfade.as_secs_f32();
        let duration = self.sh().duration;
        next_ready && duration > 3.0 * fade && duration - self.pos() <= fade
    }

    /// Прошлый трек уходит в отдельный звуковой поток и затухает, а следующий начинается в новом.
    fn start_crossfade(&mut self) {
        let Some((_, handle, sink)) = &mut self.audio else { return };
        let fresh = match Sink::try_new(handle) {
            Ok(s) => s,
            Err(e) => return log(format!("sink: {e}")),
        };
        let old = std::mem::replace(sink, fresh);
        self.fade_out.start(self.crossfade);
        self.fade_in = self.crossfade;
        // advance() глушит затухающий поток через stop_audio(), поэтому он отдаётся после.
        self.advance(End::Finished);
        self.fading = Some(old);
    }

    fn pos(&self) -> f32 {
        match self.sink() {
            Some(s) => s.get_pos().as_secs_f32(),
            None => self.held_pos.unwrap_or(0.0),
        }
    }

    fn playback_position(&self) -> f32 {
        if self.loaded { return self.pos(); }
        self.resume_at.as_ref().filter(|(id, _)| self.current.as_ref().is_some_and(|t| t.id == *id))
            .map_or(0.0, |(_, position)| *position)
    }

    /// Звук открыт на одном устройстве и сам за выходом Windows не переходит. Раз в секунду
    /// сверяем выход по умолчанию: сменился или пропал — переоткрываем.
    #[cfg(windows)]
    fn check_output(&mut self) {
        if self.audio.is_none() || self.out_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.out_checked = Instant::now();
        let now = default_output_name();
        if now == self.out_name {
            return;
        }
        log(format!("аудиовыход: {} → {}", self.out_name.as_deref().unwrap_or("нет"), now.as_deref().unwrap_or("нет")));
        let pos = self.pos();
        self.reopen_output(pos, now.is_some());
    }

    /// Трек играет и скачан целиком, а позиция стоит: выход умер молча. На Linux так бывает
    /// после блокировки экрана (#49).
    fn check_stall(&mut self) {
        let pos = self.pos();
        let playing = self.loaded
            && !self.paused
            && self.current_buf.as_ref().is_some_and(|b| b.done())
            && self.sink().is_some_and(|s| !s.empty());
        if !playing || pos != self.stall.0 {
            self.stall = (pos, Instant::now());
            return;
        }
        if self.stall.1.elapsed() < STALL_LIMIT {
            return;
        }
        log(format!("звук встал на {pos:.1} с, выход переоткрывается"));
        self.stall = (pos, Instant::now());
        self.reopen_output(pos, true);
    }

    /// Закрыть выход и продолжить трек с `pos` на новом. Не вышло — пауза до Play с того же места.
    fn reopen_output(&mut self, pos: f32, available: bool) {
        self.audio = None;
        self.fading = None;
        #[cfg(windows)]
        {
            self.out_name = None;
        }
        if !self.loaded {
            return;
        }
        if !available || !self.restore_at(pos) {
            self.held_pos = Some(pos);
            if !self.paused {
                self.paused = true;
                if let Some(l) = &mut self.listen {
                    l.stop();
                    l.pause = true;
                }
            }
            (self.notify)();
        }
    }

    /// Открыть выход заново и продолжить текущий трек из скачанного буфера с места `pos`.
    fn restore_at(&mut self, pos: f32) -> bool {
        let Some(buf) = self.current_buf.clone() else { return false };
        if !self.ensure_audio() {
            return false;
        }
        let Ok(src) = open_decoder(&buf) else { return false };
        let src = self.source(src, Duration::ZERO);
        let (vol, paused) = (self.gain(), self.paused);
        let Some(sink) = self.sink() else { return false };
        sink.pause();
        sink.set_volume(vol);
        sink.append(src);
        if let Err(e) = sink.try_seek(Duration::from_secs_f32(pos)) {
            log(format!("seek: {e:?}"));
        }
        if !paused {
            sink.play();
        }
        self.held_pos = None;
        true
    }

    fn error(&self, e: impl Into<String>) {
        let e = e.into();
        log(format!("error: {e}"));
        self.sh().error = Some((e, Instant::now()));
        (self.notify)();
    }

    fn feedback(&mut self, kind: &'static str, track: Option<&Track>, played: Option<f64>) {
        self.send_feedback(kind, track, played, true);
    }

    /// `from` волны, как у оригинала: «Моя волна» с главной или волна по исполнителю, альбому…
    fn rotor_from(&self) -> String {
        match &self.source {
            Source::Seed { station, .. } => format!("desktop-radio-{}-default", station.replace(':', "-")),
            _ => WAVE_FROM.to_string(),
        }
    }

    /// Фидбек волне. Пропуск, дизлайк и «дослушал» при `defer` ждут следующего запроса треков
    /// и уходят вместе с ним, как у оригинала; «назад» он отправляет сразу.
    fn send_feedback(&mut self, kind: &'static str, track: Option<&Track>, played: Option<f64>, defer: bool) {
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        if self.session.is_empty() && kind == "radioStarted" {
            self.radio_started_pending = true;
            return;
        }
        // Фидбек идёт с пакетом самого трека: следующий пакет часто приходит, пока доигрывает прошлый.
        let batch = track
            .map(|t| t.batch_id.clone())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| self.batch_id.clone());
        let length = track
            .filter(|t| kind == "trackFinished" && t.duration_ms > 0)
            .map(|t| t.duration_ms as f64 / 1000.0);
        let track = track.map(|t| t.full_id());
        let session = self.session.clone();
        let from = self.rotor_from();
        if session.is_empty() {
            let _ = self.bg.send(Box::new(move || {
                if let Err(e) = api.feedback(&station, kind, track.as_deref(), played, &batch, &from) {
                    log(format!("feedback {kind}: {e}"));
                }
            }));
            return;
        }
        let body = crate::api::session_event(kind, track.as_deref(), played, length, &batch, &from);
        if defer && matches!(kind, "skip" | "dislike" | "trackFinished") {
            self.deferred.push(body);
            return;
        }
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = api.session_feedback(&session, &body) {
                log(format!("feedback {kind}: {e}"));
            }
        }));
    }

    /// Отложенные фидбеки, которые не ушли с запросом треков, отправляются отдельно.
    fn flush_feedback(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let list = std::mem::take(&mut self.deferred);
        let (Some(api), false) = (self.api.clone(), self.session.is_empty()) else { return };
        let session = self.session.clone();
        let _ = self.bg.send(Box::new(move || {
            for body in list {
                if let Err(e) = api.session_feedback(&session, &body) {
                    log(format!("feedback: {e}"));
                }
            }
        }));
    }

    /// Контекст `/plays` для трека, который сейчас заиграл.
    fn play_context(&self) -> PlayContext {
        let radio = |from: String, item: &str| PlayContext {
            from,
            context: "radio",
            item: Some(item.to_string()),
            playlist: None,
            session: self.session.clone(),
        };
        let list = |from: &str, context: &'static str, item: Option<String>, playlist: Option<String>| PlayContext {
            from: from.to_string(),
            context,
            item,
            playlist,
            session: String::new(),
        };
        match &self.source {
            Source::Wave => radio(WAVE_FROM.to_string(), WAVE),
            Source::Seed { station, .. } => radio(self.rotor_from(), station),
            Source::List { kind, id, .. } => match kind {
                ListKind::Album => list("desktop-album-album-default", "album", Some(id.clone()), None),
                ListKind::Artist => list("desktop-artist-artist-default", "artist", Some(id.clone()), None),
                ListKind::Playlist => list("desktop-playlist-playlist-default", "playlist", Some(id.clone()), Some(id.clone())),
                ListKind::Likes => {
                    let likes = format!("{}:3", self.uid);
                    list("desktop-playlist-playlist-default", "playlist", Some(likes.clone()), Some(likes))
                }
                ListKind::Tracks if id == "search" => list("desktop-search-search-default", "search", None, None),
                ListKind::Tracks => list("desktop-other-other-default", "other", None, None),
            },
        }
    }

    /// Трек стал текущим: новое прослушивание. Время слушания идёт, только когда трек звучит.
    fn begin_listen(&mut self, track: &Track, repeated: bool) {
        let ctx = self.play_context();
        let radio = ctx.context == "radio";
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        self.listen = Some(Listen {
            track: track.clone(),
            ctx,
            play_id: uuid4(),
            add_time: format!("{:016}-{}", random_u128() % 10_000_000_000_000_000, ms.div_ceil(1000)),
            start: crate::api::iso_now(),
            played: 0.0,
            since: (self.loaded && !self.paused).then(Instant::now),
            pause: false,
            // В волне оригинал всегда шлёт seek: при старте трека плеер ставит его на начало.
            seek: radio,
            repeated,
        });
    }

    /// Трек сменился: фидбек волне (если нужен) и запись в `/plays`, как у оригинала.
    /// Второй раз для того же прослушивания ничего не шлёт.
    fn end_current(&mut self, fb: Option<&'static str>, defer: bool, reason: &'static str) {
        let Some(mut l) = self.listen.take() else { return };
        l.stop();
        let len = l.track.duration_ms as f64 / 1000.0;
        let near = |x: f64| if (len - x).abs() < 0.5 { len } else { round3(x) };
        let played = near(l.played());
        let end = if reason == "finish" { len } else { near(self.pos() as f64) };
        if let Some(kind) = fb {
            let t = l.track.clone();
            self.send_feedback(kind, Some(&t), Some(played), defer);
        }
        let (Some(api), false) = (self.api.clone(), self.uid.is_empty()) else { return };
        let t = &l.track;
        let mut play = serde_json::json!({
            "playId": l.play_id,
            "from": l.ctx.from,
            "totalPlayedSeconds": played,
            "endPositionSeconds": end,
            "trackLengthSeconds": len,
            "timestamp": crate::api::iso_now(),
            "context": l.ctx.context,
            "contextItem": l.ctx.item,
            "addTracksToPlayerTime": l.add_time,
            "fromCache": false,
            "isRestored": false,
            "audioAuto": "none",
            "audioOutputName": "Динамик",
            "audioOutputType": "Speaker",
            "trackId": t.id,
            "seek": l.seek,
            "pause": l.pause,
            "startTimestamp": l.start,
            "maxPlayerStage": "play",
            "isRepeated": l.repeated,
            "changeReason": reason,
            "isLivePlayableIndex": l.ctx.context == "radio",
        });
        if let Some(a) = &t.album_id {
            play["albumId"] = a.clone().into();
        }
        if let Some(p) = &l.ctx.playlist {
            play["playlistId"] = p.clone().into();
        }
        if l.ctx.context == "radio" {
            let batch = if t.batch_id.is_empty() { self.batch_id.clone() } else { t.batch_id.clone() };
            play["radioSessionId"] = l.ctx.session.clone().into();
            play["batchId"] = batch.into();
            play["isFromAutoflow"] = false.into();
            play["isFromPumpkin"] = false.into();
        }
        let uid = self.uid.clone();
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = api.plays(&uid, &play) {
                log(format!("plays: {e}"));
            }
        }));
    }

    /// queue для следующего пакета, как у оригинала: трек, на котором сменился трек, и тот,
    /// что заиграл после него. Всю историю оригинал туда не кладёт.
    fn rotor_queue(&self) -> Vec<String> {
        let mut q: Vec<String> = self.history.last().filter(|t| !t.batch_id.is_empty()).map(|t| t.full_id()).into_iter().collect();
        if let Some(c) = &self.current {
            q.push(c.full_id());
        }
        q
    }

    fn stop_audio(&mut self) {
        self.current_load = None;
        self.paused_start = None;
        if let Some(s) = self.sink() {
            s.clear();
        }
        self.fading = None;
        self.loaded = false;
        self.held_pos = None;
    }

    fn clear_queue(&mut self) {
        self.flush_feedback();
        self.next_check = None;
        self.queue.clear();
        self.prefetch = None;
        self.prefetching = None;
        self.prefetch_load = None;
        self.prefetch_failure = None;
        self.prefetch_gen = self.prefetch_gen.wrapping_add(1);
        self.fetching = false;
        self.batch_gen += 1;
        self.retry_at = None;
        self.batch_id.clear();
        self.session.clear();
    }

    fn reset(&mut self) {
        self.stop_audio();
        self.current_check = None;
        self.gen += 1;
        self.login_gen = self.login_gen.wrapping_add(1);
        self.clear_queue();
        self.history.clear();
        self.listen = None;
        self.played_station.clear();
        self.wave_restored = false;
        self.clone_pending = false;
        self.restore_last = None;
        self.current = None;
        self.current_buf = None;
        self.catalog_buf = None;
        self.list = None;
        self.source = Source::Wave;
        let mut s = self.sh();
        s.track = None;
        s.source = Source::Wave;
        s.loading = false;
    }

    fn set_source(&mut self, src: Source) {
        self.source = src.clone();
        self.sh().source = src;
    }

    /// Состояние движка → Shared для UI.
    fn sync(&mut self) {
        let playing = self.loaded && !self.paused && self.sink().map(|s| !s.is_paused()).unwrap_or(false);
        let pos = self.playback_position();
        let upcoming: Vec<Track> = self.queue.iter().take(30).cloned().collect();
        let mut s = self.shared.lock().unwrap();
        s.playing = playing;
        let preparing = if self.current.is_some() { s.loading } else { self.fetching };
        s.waiting_for_playback = !self.paused && preparing;
        if s.seek_request == self.handled_seek || s.track.is_none() {
            s.position = pos;
        }
        s.volume = self.volume;
        s.repeat = self.repeat;
        s.shuffle = self.shuffle;
        s.upcoming = upcoming;
    }

    // ---------- главный цикл ----------

    fn tick(&mut self) {
        if let Some(id) = &self.prefetching {
            let wanted = self.queue.front().is_some_and(|t| t.id == *id)
                || (!self.loaded && self.current.as_ref().is_some_and(|t| t.id == *id));
            if !wanted {
                self.prefetch_load = None;
                self.prefetching = None;
                self.prefetch_gen = self.prefetch_gen.wrapping_add(1);
            }
        } else if self.prefetch.is_none() {
            self.prefetch_load = None;
        }
        self.check_replacement();
        #[cfg(windows)]
        self.check_output();
        self.check_stall();
        // Пока играет, позиция уходит в Ynison раз в минуту: закроют приложение — потеряется не больше.
        let playing = self.loaded && !self.paused;
        if self.ynison_due.is_some_and(|at| Instant::now() >= at) || (playing && self.ynison_at.elapsed() >= YNISON_EVERY) {
            self.publish();
        }
        if let Some(at) = self.retry_at {
            if Instant::now() >= at {
                self.retry_at = None;
                self.fetch_batch();
            }
        }
        if self.fading.as_ref().is_some_and(|s| s.empty()) {
            self.fading = None;
        }
        if self.crossfade_due() {
            self.start_crossfade();
            self.sync();
            (self.notify)();
            return;
        }
        if self.loaded && !self.paused && self.sink().map(|s| s.empty()).unwrap_or(false) {
            if self.repeat == Repeat::One && self.current_buf.is_some() {
                if let Some(l) = &mut self.listen {
                    l.repeated = true;
                }
                self.end_current(None, true, "finish");
                if self.replay() {
                    if let Some(t) = self.current.clone() {
                        self.begin_listen(&t, false);
                    }
                    return;
                }
            }
            self.advance(End::Finished);
            self.sync();
            (self.notify)();
            return;
        }
        self.sync();
        if self.loaded && self.prefetch.is_none() && self.prefetching.is_none() {
            self.prefetch_next();
        }
        let playing = self.sh().playing;
        if playing != self.was_playing {
            self.was_playing = playing;
            (self.notify)();
        }
    }

    fn replay(&mut self) -> bool {
        let Some(buf) = self.current_buf.clone() else { return false };
        let Ok(src) = open_decoder(&buf) else { return false };
        let src = self.source(src, Duration::ZERO);
        if let Some(sink) = self.sink() {
            sink.append(src);
            sink.play();
        }
        true
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(c) => self.cmd(c),
            Msg::Seek { seconds, track_generation, request } => {
                if self.gen == track_generation && self.sh().seek_request == request {
                    self.cmd(Cmd::Seek(seconds));
                }
                self.handled_seek = request;
            }
            Msg::Account { gen, .. } if gen != self.login_gen => {}
            Msg::Account { gen, res } => match res {
                Ok(acc) => {
                    self.uid = acc.uid.clone();
                    {
                        let mut s = self.sh();
                        s.status = Status::Ready;
                        s.uid = acc.uid.clone();
                        s.login = acc.login;
                        s.has_plus = acc.has_plus;
                        s.error = None;
                    }
                    self.feedback("radioStarted", None, None);
                    if let Some(api) = self.api.clone() {
                        let tx = self.tx.clone();
                        // Волну качаем, когда станет ясно, не продолжить ли то, что играло.
                        let (resume_api, resume_tx) = (api.clone(), tx.clone());
                        thread::spawn(move || {
                            let _ = resume_tx.send(Msg::Resume { gen, res: fetch_resume(&resume_api) });
                        });
                        let uid = acc.uid;
                        thread::spawn(move || {
                            let ids = api.liked_ids(&uid);
                            let _ = tx.send(Msg::Likes(uid, ids));
                        });
                    }
                }
                Err(e) => {
                    log(format!("account: {e}"));
                    self.api = None;
                    self.sh().status = Status::Failed(e);
                }
            },
            Msg::Resume { gen, .. } if gen != self.login_gen => {}
            Msg::Resume { res, .. } => match res {
                Ok(Some(r)) if self.paused && self.current.is_none() => self.apply_resume(r),
                Ok(_) => self.fetch_batch(),
                Err(e) => {
                    log(format!("продолжение: {e}"));
                    self.fetch_batch();
                }
            },
            Msg::Likes(uid, _) if uid != self.uid => {}
            Msg::Likes(_, res) => match res {
                Ok(ids) => self.merge_liked_ids(ids),
                Err(e) => log(format!("likes: {e}")),
            },
            Msg::LikeFinished { uid, id, liked, result } => {
                if uid != self.uid { return; }
                if let Some(write) = self.like_writes.get_mut(&id).filter(|w| w.pending > 0) {
                    if result.is_ok() { write.saved = liked; }
                    write.pending -= 1;
                    if write.pending == 0 {
                        let mut likes = self.likes.lock().unwrap();
                        let changed = if write.saved { likes.insert(id) } else { likes.remove(&id) };
                        drop(likes);
                        if changed { self.likes_changed(); }
                    }
                }
                if let Err(e) = result { self.error(format!("Лайк не сохранился: {e}")); }
            }
            Msg::Batch { res, replace, gen } => {
                if gen != self.batch_gen {
                    return;
                }
                self.fetching = false;
                match res {
                    Ok(b) => self.on_batch(b, replace),
                    Err(e) => {
                        self.error(format!("Не удалось получить треки волны: {e}"));
                        self.retry_at = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
            }
            Msg::Audio { gen, res } => {
                if gen == self.gen {
                    match res {
                        Ok(b) => self.play_loaded(b),
                        Err(e) => self.on_load_error(e),
                    }
                }
            }
            Msg::KnownFinished { owner, revision, audio } => {
                let authorized = revision == crate::resolver_login::session_revision() && replacements_enabled();
                if self.current_load.as_ref().is_some_and(|job| owner.ptr_eq(&Arc::downgrade(job))) {
                    if let Some(mut loaded) = self.paused_start.take() {
                        loaded.known = audio.filter(|_| authorized).map_or(KnownStatus::Miss(revision), |a| KnownStatus::Verified(revision, a));
                        self.play_loaded(loaded);
                        return;
                    }
                    if !authorized { return; }
                    if let Some(check) = self.current_check.as_mut().filter(|c| owner.ptr_eq(&Arc::downgrade(&c.owner))) {
                        if audio.is_some() {
                            check.finished = true;
                            check.audio = audio;
                        } else {
                            self.current_check = None; // Let the regular background verification run.
                        }
                    }
                } else if authorized && self.prefetch_load.as_ref().is_some_and(|job| owner.ptr_eq(&Arc::downgrade(job))) {
                    if let Some((_, loaded)) = &mut self.prefetch {
                        loaded.known = audio.map_or(KnownStatus::Miss(revision), |a| KnownStatus::Verified(revision, a));
                    }
                }
            }
            Msg::Replacement { job, audio } => {
                let revision = crate::resolver_login::session_revision();
                if let Some(check) = self.current_check.iter_mut().chain(self.next_check.iter_mut())
                    .find(|c| c.revision == revision && job.ptr_eq(&Arc::downgrade(&c.owner))) {
                    check.audio = audio;
                    check.finished = true;
                }
            }
            Msg::Prefetch { gen, id, res } => {
                if gen != self.prefetch_gen || self.prefetching.as_deref() != Some(id.as_str()) {
                    return;
                }
                self.prefetching = None;
                let waiting = !self.loaded && self.current.as_ref().map(|t| t.id == id).unwrap_or(false);
                match res {
                    Ok(b) if waiting => self.play_loaded(b),
                    Ok(b) if self.queue.front().is_some_and(|t| t.id == id) => self.prefetch = Some((id, b)),
                    Ok(_) => {}
                    Err(e) => {
                        log(format!("prefetch {id}: {e}"));
                        self.prefetch_failure = Some((id, Instant::now()));
                        if waiting {
                            self.start_download();
                        }
                    }
                }
            }
            Msg::Error(e) => self.error(e),
        }
    }

    fn cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Login(token) => {
                self.reset();
                self.like_writes.clear();
                self.likes.lock().unwrap().clear();
                self.likes_changed();
                let api = Api::new(&token);
                self.api = Some(api.clone());
                {
                    let mut s = self.sh();
                    s.status = Status::Connecting;
                    s.error = None;
                }
                let tx = self.tx.clone();
                let gen = self.login_gen;
                thread::spawn(move || {
                    let _ = tx.send(Msg::Account { gen, res: api.account() });
                });
            }
            Cmd::Logout => {
                self.reset();
                self.api = None;
                self.uid.clear();
                self.likes.lock().unwrap().clear();
                self.like_writes.clear();
                self.likes_changed();
                let mut s = self.sh();
                s.status = Status::LoggedOut;
                s.login.clear();
                s.uid.clear();
            }
            Cmd::Resume => self.resume(),
            Cmd::Leave(done) => {
                if let (Some(api), Some(now)) = (self.api.clone(), self.ynison_now()) {
                    if let Err(e) = crate::ynison::publish(api.token(), &crate::ynison::device_id(), &now) {
                        log(format!("ynison: {e}"));
                    }
                }
                let _ = done.send(());
            }
            Cmd::PlayPause if self.paused => self.resume(),
            Cmd::PlayPause => {
                self.paused = true;
                if let Some(l) = &mut self.listen {
                    l.stop();
                    l.pause = true;
                }
                if let Some(sink) = self.sink() { sink.pause(); }
                self.fading = None;
                self.publish_soon();
            }
            Cmd::Next => {
                self.errors_in_row = 0;
                if self.current.is_some() {
                    self.advance(End::Skip);
                }
            }
            Cmd::Prev => self.prev(),
            Cmd::Seek(sec) => {
                if let Some(l) = &mut self.listen {
                    l.seek = true;
                }
                self.fading = None;
                if self.loaded {
                    if let Some(s) = self.sink() {
                        if let Err(e) = s.try_seek(Duration::from_secs_f32(sec.max(0.0))) {
                            log(format!("seek: {e:?}"));
                        }
                    } else if self.held_pos.is_some() {
                        self.held_pos = Some(sec.max(0.0));
                    }
                }
                self.publish_soon();
            }
            Cmd::Volume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                self.apply_volume();
            }
            Cmd::Loudness(l) => {
                self.loudness = l;
                self.apply_volume();
            }
            Cmd::LikeTrack(t, saved) => self.toggle_like(t, saved),
            Cmd::Dislike => {
                if let (Some(api), Some(t)) = (self.api.clone(), self.current.as_ref()) {
                    let id = t.full_id();
                    let uid = self.uid.clone();
                    let tx = self.tx.clone();
                    let _ = self.bg.send(Box::new(move || {
                        if let Err(e) = api.dislike(&uid, &id) {
                            let _ = tx.send(Msg::Error(format!("Дизлайк не сохранился: {e}")));
                        }
                    }));
                }
                self.advance(End::Dislike);
            }
            Cmd::Wave(src) => self.start_wave(src, true),
            Cmd::TuneWave(settings) => {
                self.start_wave(Source::Wave, false);
                self.session.clear();
                self.clone_pending = false;
                self.restore_last = None;
                // Сессию с прошлого запуска уже не подхватывать: волна начинается заново.
                self.wave_restored = true;
                self.tune = Some(settings);
                self.fetch_batch_with(true);
            }
            Cmd::PlayList { source, tracks, start, after } => {
                if tracks.is_empty() {
                    return;
                }
                // Играть нечего: текущий трек не трогаем, а говорим, почему ничего не включилось.
                if !crate::api::allow_explicit() && tracks.iter().all(|t| t.explicit) {
                    self.error("Здесь только треки 18+. Их можно включить в меню профиля");
                    return;
                }
                self.end_current(Some("skip"), true, "other_track");
                self.clear_queue();
                let start = start.min(tracks.len() - 1);
                let mut order: Vec<Track> = tracks.clone();
                order.rotate_left(start);
                let first = order.remove(0);
                if self.shuffle {
                    shuffle(&mut order);
                }
                self.queue.push_back(first);
                self.queue.extend(order);
                self.list = Some(ListPlay { deferred: VecDeque::new(), all: tracks, after });
                self.set_source(source);
                self.advance(End::Skip);
            }
            Cmd::Jump(i) => {
                if i >= self.queue.len() {
                    return;
                }
                let skipped: Vec<Track> = self.queue.drain(..i).collect();
                self.end_current(Some("skip"), true, "other_track");
                if let Some(cur) = self.current.take() {
                    self.history.push(cur);
                }
                self.history.extend(skipped);
                self.trim_history();
                self.stop_audio();
                self.play_from_queue();
                self.refresh_wave();
                self.flush_feedback();
            }
            Cmd::Repeat(r) => self.repeat = r,
            Cmd::Enqueue { track, next } => {
                if next {
                    self.queue.push_front(track);
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                } else {
                    self.queue.push_back(track);
                }
            }
            Cmd::AllowExplicit(allow) => self.set_allow_explicit(allow),
            Cmd::SourcesModeChanged => self.sources_mode_changed(crate::resolver_login::mode_enabled()),
            Cmd::Crossfade(over) => self.crossfade = over,
            Cmd::Shuffle(on) => {
                self.shuffle = on;
                if self.list.is_some() {
                    let mut v: Vec<Track> = self.queue.drain(..).collect();
                    if on {
                        shuffle(&mut v);
                    } else if let (Some(l), Some(cur)) = (&self.list, &self.current) {
                        let i = l.all.iter().position(|t| t.id == cur.id).map(|i| i + 1).unwrap_or(0);
                        v = l.all[i.min(l.all.len())..].to_vec();
                    }
                    self.queue.extend(v);
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                }
            }
        }
    }

    fn likes_changed(&self) {
        let mut s = self.sh();
        s.likes_revision = s.likes_revision.wrapping_add(1);
    }

    fn merge_liked_ids(&self, ids: Vec<String>) {
        let mut likes = self.likes.lock().unwrap();
        let before = likes.len();
        likes.extend(ids.into_iter().filter(|id| !self.like_writes.contains_key(id)));
        let changed = likes.len() != before;
        drop(likes);
        if changed { self.likes_changed(); }
    }

    fn toggle_like(&mut self, t: Track, done: Option<LikeCompleted>) {
        let Some(api) = self.api.clone() else { return };
        let remove = {
            let mut l = self.likes.lock().unwrap();
            let had = l.remove(&t.id);
            if !had {
                l.insert(t.id.clone());
            }
            had
        };
        self.likes_changed();
        self.like_writes.entry(t.id.clone()).or_insert(LikeWrite { pending: 0, saved: remove }).pending += 1;
        let (uid, track, tx) = (self.uid.clone(), t.clone(), self.tx.clone());
        let _ = self.bg.send(Box::new(move || {
            let result = api.like(&uid, &track.full_id(), remove);
            if let Some(done) = done { done(&track, !remove, &result); }
            let _ = tx.send(Msg::LikeFinished { uid, id: track.id, liked: !remove, result });
        }));
        // Лайк трека из волны оригинал сообщает и самой волне, чтобы она его учла.
        if self.session.is_empty() {
            return;
        }
        let in_wave = self
            .current
            .iter()
            .chain(self.queue.iter())
            .chain(self.history.iter())
            .find(|w| w.id == t.id && !w.batch_id.is_empty())
            .cloned();
        if let Some(w) = in_wave {
            self.feedback(if remove { "unlike" } else { "like" }, Some(&w), None);
        }
    }

    // ---------- очередь ----------

    fn start_wave(&mut self, src: Source, fetch: bool) {
        // Пропуск уходит ещё в старую волну: clear_queue отправит его туда.
        self.end_current(Some("skip"), true, "other_track");
        self.clear_queue();
        self.list = None;
        self.set_source(src);
        self.feedback("radioStarted", None, None);
        if fetch {
            self.fetch_batch_with(true);
        }
    }

    fn fetch_batch(&mut self) {
        self.fetch_batch_with(false);
    }

    fn fetch_batch_with(&mut self, replace: bool) {
        if self.fetching || self.list.is_some() {
            return;
        }
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        self.fetching = true;
        self.retry_at = None;
        if self.played_station != station {
            self.played_station = station.clone();
            if station == WAVE {
                self.restore_wave();
            }
        }
        let (session, queue) = if let Some(settings) = self.tune.take() {
            (WaveSession::Tuned(settings), Vec::new())
        } else if self.session.is_empty() {
            (WaveSession::New, if replace { Vec::new() } else { self.rotor_queue() })
        } else if self.clone_pending {
            (WaveSession::Clone(self.session.clone()), self.restore_last.clone().into_iter().collect())
        } else {
            (WaveSession::Next(self.session.clone()), self.rotor_queue())
        };
        let feedbacks = std::mem::take(&mut self.deferred);
        let tx = self.tx.clone();
        let gen = self.batch_gen;
        // В очереди фонового потока: запрос уйдёт после уже отправленных фидбеков.
        let _ = self.bg.send(Box::new(move || {
            let res = api.wave_session(&station, session, &queue, &feedbacks);
            let _ = tx.send(Msg::Batch { res, replace, gen });
        }));
    }

    fn on_batch(&mut self, b: Batch, replace: bool) {
        self.batch_id = b.batch_id;
        self.clone_pending = false;
        self.restore_last = None;
        if !b.session.is_empty() {
            // Сессия могла подхватиться с прошлого запуска — radioStarted нужен и ей.
            let changed = b.session != self.session;
            self.session = b.session;
            if std::mem::take(&mut self.radio_started_pending) {
                self.feedback("radioStarted", None, None);
            }
            if changed {
                self.save_wave();
            }
        }
        self.merge_liked_ids(b.liked);
        if replace {
            self.queue.clear();
            self.prefetch = None;
        }
        // Пакет берётся как есть, как у оригинала: сервер сам решает, что повторить.
        // Убираем только то, что уже стоит в очереди или играет, и 18+, если их выключили.
        let queued = |t: &Track| {
            self.queue.iter().any(|q| q.id == t.id) || (!replace && self.current.as_ref().is_some_and(|c| c.id == t.id))
        };
        let no_explicit = !crate::api::allow_explicit();
        let tracks: Vec<Track> = b.tracks.iter().filter(|t| !queued(t) && !(no_explicit && t.explicit)).cloned().collect();
        self.queue.extend(tracks);
        if replace {
            self.advance(End::Restart);
        } else if self.current.is_none() {
            self.play_from_queue();
        } else if self.loaded {
            self.prefetch_next();
        }
    }

    fn trim_history(&mut self) {
        if self.history.len() > 50 {
            let n = self.history.len() - 50;
            self.history.drain(..n);
        }
    }

    /// Смена трека. Фидбек «дослушал» и «пропуск» ждёт следующего запроса треков и уходит
    /// вместе с ним, как у оригинала.
    fn advance(&mut self, end: End) {
        let (kind, reason) = match end {
            End::Finished => ("trackFinished", "finish"),
            End::Skip => ("skip", "skip"),
            End::Dislike => ("dislike", "dislike"),
            End::Restart => ("skip", "other_track"),
        };
        self.end_current(Some(kind), true, reason);
        if let Some(cur) = self.current.take() {
            self.history.push(cur);
            self.trim_history();
        }
        self.stop_audio();
        self.play_from_queue();
        if matches!(end, End::Skip | End::Dislike) {
            self.refresh_wave();
        }
    }

    /// Пропуск в волне, как у оригинала: заиграл следующий трек, остальные несыгранные треки
    /// волны убираются и сразу просятся новые, а пропуск уходит в том же запросе. Так волна
    /// подстраивается сразу, а не через пакет. Треки, добавленные вручную, остаются.
    fn refresh_wave(&mut self) {
        if self.list.is_some() || self.source.station().is_none() || self.session.is_empty() {
            return;
        }
        self.queue.retain(|t| t.batch_id.is_empty());
        if let Some((id, _)) = &self.prefetch {
            if !self.queue.iter().any(|t| t.id == *id) {
                self.prefetch = None;
            }
        }
        self.fetch_batch();
    }

    /// Список закончился: по кругу (исполнитель) или волна по нему (плейлист, альбом).
    fn list_finished(&mut self) {
        let Some(l) = self.list.take() else { return };
        match l.after {
            After::Loop => {
                let mut all = l.all.clone();
                if self.shuffle {
                    shuffle(&mut all);
                }
                self.queue.extend(all);
                self.list = Some(l);
            }
            After::Wave(src) => {
                self.flush_feedback();
                self.set_source(src);
                self.session.clear();
                self.feedback("radioStarted", None, None);
            }
        }
    }

    fn sources_mode_changed(&mut self, enabled: bool) {
        self.current_lookup_revision = None;
        self.current_check = None;
        self.next_check = None;
        if enabled { return; }
        self.fading = None;
        if let Some(mut loaded) = self.paused_start.take() {
            loaded.known = KnownStatus::Miss(crate::resolver_login::session_revision());
            self.play_loaded(loaded);
            return;
        }
        let Some((catalog, format)) = self.catalog_buf.clone() else { return };
        if !self.loaded || self.current_buf.as_ref().is_none_or(|buf| Arc::ptr_eq(buf, &catalog)) { return; }
        let position = self.pos();
        if let Some(sink) = self.sink() { sink.clear(); }
        self.current_buf = Some(catalog);
        self.sh().format = format;
        if self.audio.is_none() { return; }
        if !self.restore_at(position) {
            self.held_pos = Some(position);
            self.paused = true;
        }
    }

    /// Как у оригинала при смене настроек волны: текущий трек доигрывает (если он не 18+, а их
    /// выключили), а несыгранные треки волны заменяет новая сессия уже с этим флагом.
    fn set_allow_explicit(&mut self, allow: bool) {
        crate::api::ALLOW_EXPLICIT.store(allow, std::sync::atomic::Ordering::Relaxed);
        let wave = self.list.is_none() && self.source.station().is_some();
        let skip_current = !allow && self.current.as_ref().is_some_and(|t| t.explicit);
        if wave {
            self.queue.retain(|t| t.batch_id.is_empty());
        }
        if !allow {
            if self.blocked_list() {
                if skip_current {
                    self.queue.push_front(self.current.as_ref().unwrap().clone());
                }
                self.defer_explicit();
            } else {
                self.queue.retain(|t| !t.explicit);
            }
        }
        self.prefetch = None;
        self.prefetching = None;
        self.prefetch_load = None;
        self.prefetch_failure = None;
        self.prefetch_gen = self.prefetch_gen.wrapping_add(1);
        self.next_check = None;
        self.current_check = None;
        // A pending replacement (including a promoted prefetch) used the previous setting.
        if !skip_current && self.current.is_some() && self.sh().loading {
            self.gen += 1;
            self.sh().track_generation = self.gen;
            self.start_download();
        }
        if skip_current {
            self.end_current(Some("skip"), true, "skip");
            if let Some(cur) = self.current.take() {
                self.history.push(cur);
                self.trim_history();
            }
            self.stop_audio();
        }
        if wave {
            self.flush_feedback();
            self.session.clear();
            self.fetching = false;
            self.batch_gen += 1;
        }
        if skip_current {
            self.play_from_queue();
        }
        if wave {
            self.fetch_batch();
        }
    }

    fn blocked_list(&self) -> bool {
        !crate::api::allow_explicit()
            && self.list.as_ref().is_some_and(|list| !list.all.is_empty() && list.all.iter().all(|t| t.explicit))
    }

    fn defer_explicit(&mut self) {
        if let Some(list) = &mut self.list {
            self.queue.retain(|track| {
                if track.explicit { list.deferred.push_back(track.clone()); }
                !track.explicit
            });
        }
    }

    fn resume(&mut self) {
        self.paused = false;
        self.publish_soon();
        if let Some(loaded) = self.paused_start.take() {
            // Both bounded waits have already elapsed. A user's Play must work immediately.
            self.play_loaded(loaded);
            return;
        }
        if let Some(pos) = self.held_pos.filter(|_| self.loaded && self.audio.is_none()) {
            if !self.restore_at(pos) {
                self.paused = true;
                self.error(NO_OUTPUT);
                return;
            }
        }
        let loaded = self.loaded;
        if let Some(l) = self.listen.as_mut().filter(|_| loaded) {
            l.go();
        }
        if self.current.is_none() {
            if self.list.is_some() {
                self.play_from_queue();
                return;
            }
            if self.sh().status == Status::Ready { self.fetch_batch(); }
        }
        if let Some(sink) = self.sink() { sink.play(); }
    }

    fn play_from_queue(&mut self) {
        let blocked_list = self.blocked_list();
        if blocked_list {
            self.defer_explicit();
        } else if crate::api::allow_explicit() {
            if let Some(list) = &mut self.list { self.queue.append(&mut list.deferred); }
        } else {
            self.queue.retain(|t| !t.explicit);
        }
        if self.queue.is_empty() && self.list.is_some() && !blocked_list {
            match (&self.list, self.repeat) {
                (Some(l), Repeat::All) => {
                    let all = l.all.clone();
                    self.queue.extend(all);
                }
                _ => self.list_finished(),
            }
        }
        if !crate::api::allow_explicit() {
            self.queue.retain(|t| !t.explicit);
        }
        match self.queue.pop_front() {
            Some(t) => {
                self.current = Some(t);
                self.start_current();
            }
            None => {
                self.current = None;
                self.current_check = None;
                self.next_check = None;
                self.current_buf = None;
                self.catalog_buf = None;
                if self.list.is_some() { self.paused = true; }
                self.gen += 1;
                let mut s = self.sh();
                s.track = None;
                s.loading = self.list.is_none() && self.source.station().is_some();
            }
        }
        // Как у оригинала: новый пакет волны нужен, когда заиграл последний трек очереди.
        if self.queue.is_empty() {
            self.fetch_batch();
        }
    }

    fn prev(&mut self) {
        if self.loaded && self.pos() > 4.0 {
            if let Some(s) = self.sink() {
                let _ = s.try_seek(Duration::ZERO);
            }
            return;
        }
        if !crate::api::allow_explicit() {
            self.history.retain(|t| !t.explicit);
        }
        if let Some(p) = self.history.pop() {
            self.end_current(Some("skip"), false, "back_skip");
            if let Some(c) = self.current.take() {
                self.queue.push_front(c);
            }
            self.stop_audio();
            self.current = Some(p);
            self.start_current();
        } else if self.loaded {
            if let Some(s) = self.sink() {
                let _ = s.try_seek(Duration::ZERO);
            }
        }
    }

    // ---------- загрузка ----------

    fn start_current(&mut self) {
        self.current_load = None;
        self.paused_start = None;
        self.load_started = Instant::now();
        self.gen += 1;
        self.loaded = false;
        self.listen = None;
        self.current_buf = None;
        self.catalog_buf = None;
        let Some(t) = self.current.clone() else { return };
        self.current_check = self.next_check.take().filter(|c| {
            replacements_enabled() && c.matches(&t, crate::resolver_login::session_revision())
        });
        // Как у оригинала, трек начат сразу, а не когда докачался: иначе пропуск до загрузки
        // не доходит до волны, и она отдаёт этот трек снова.
        self.begin_listen(&t, false);
        self.feedback("trackStarted", Some(&t), None);
        {
            let mut s = self.sh();
            s.track_generation = self.gen;
            s.track = Some(t.clone());
            s.position = 0.0;
            s.duration = t.duration_ms as f32 / 1000.0;
            s.format.clear();
            s.loading = true;
        }
        if self.prefetch.as_ref().map(|(id, _)| *id == t.id).unwrap_or(false) {
            let (_, b) = self.prefetch.take().unwrap();
            self.current_load = self.prefetch_load.take();
            self.play_loaded(b);
            return;
        }
        if self.prefetching.as_deref() == Some(t.id.as_str()) {
            self.current_load = self.prefetch_load.take();
            return;
        }
        self.start_download();
    }

    fn start_download(&mut self) {
        self.paused_start = None;
        let (Some(api), Some(t)) = (self.api.clone(), self.current.clone()) else { return };
        let gen = self.gen;
        let tx = self.tx.clone();
        let owner = self.current_load.insert(Arc::new(()));
        let job = Arc::downgrade(owner);
        thread::spawn(move || load_for_playback(api, t, job, tx.clone(), |res| {
            let _ = tx.send(Msg::Audio { gen, res });
        }));
    }

    fn prefetch_next(&mut self) {
        if self.paused || self.prefetching.is_some() {
            return;
        }
        let Some(next) = self.queue.front() else { return };
        let have = self.prefetch.as_ref().map(|(id, _)| *id == next.id).unwrap_or(false);
        if have || self.prefetch_failure.as_ref().is_some_and(|(id, at)| *id == next.id && at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        let next = next.clone();
        let Some(api) = self.api.clone() else { return };
        self.prefetching = Some(next.id.clone());
        let gen = self.prefetch_gen;
        let tx = self.tx.clone();
        let owner = self.prefetch_load.insert(Arc::new(()));
        let job = Arc::downgrade(owner);
        let id = next.id.clone();
        thread::spawn(move || load_for_playback(api, next, job, tx.clone(), |res| {
            let _ = tx.send(Msg::Prefetch { gen, id, res });
        }));
    }

    fn initial_audio(&mut self, track: &Track, loaded: LoadedTrack) -> (Audio, &'static str) {
        self.catalog_buf = Some((loaded.baseline.0.clone(), loaded.baseline.2.clone()));
        let revision = crate::resolver_login::session_revision();
        self.current_lookup_revision = Some(loaded.known.attempted_revision());
        if let Some(prepared) = self.take_current_replacement() {
            self.current_lookup_revision = Some(revision);
            return (prepared, "prefetch");
        }
        if replacements_enabled() && loaded.known.revision() == Some(revision) {
            if let Some(owner) = &self.current_load {
                let finished = matches!(loaded.known, KnownStatus::Verified(..));
                self.current_check = Some(ReplacementCheck {
                    id: track.id.clone(), revision, owner: owner.clone(), finished, audio: None,
                });
                if let KnownStatus::Verified(_, audio) = loaded.known { return (audio, "known"); }
            }
        }
        (loaded.baseline, "catalog")
    }

    fn play_loaded(&mut self, loaded: LoadedTrack) {
        let Some(t) = self.current.clone() else { return };
        if self.paused && matches!(loaded.known, KnownStatus::Pending(rev) if rev == crate::resolver_login::session_revision())
            && replacements_enabled() && self.current_load.is_some() {
            // A restored track may spend minutes paused. Keep the fallback ready for Play
            // and let the proven alternative be the first decoder actually queued.
            self.paused_start = Some(loaded);
            return;
        }
        let ((buf, src, format), origin) = self.initial_audio(&t, loaded);
        if !self.ensure_audio() {
            self.error("Не найдено аудиоустройство");
            self.sh().loading = false;
            return;
        }
        let fade_in = std::mem::take(&mut self.fade_in);
        let src = self.source(src, fade_in);
        let vol = self.gain();
        let paused = self.paused;
        let resume_position = self.resume_at.take().filter(|(id, pos)| *id == t.id && *pos > 1.0);
        if let Some(sink) = self.sink() {
            sink.clear();
            sink.pause();
            sink.set_volume(vol);
            sink.append(src);
            if let Some((_, pos)) = resume_position {
                if let Err(e) = sink.try_seek(Duration::from_secs_f32(pos)) { log(format!("seek: {e:?}")); }
            }
            log(format!("Первый буфер {}: {origin}, {format}, {} мс", t.id, self.load_started.elapsed().as_millis()));
            if !paused { sink.play(); }
        }
        self.current_buf = Some(buf);
        self.sh().format = format;
        self.loaded = true;
        self.held_pos = None;
        self.errors_in_row = 0;
        self.sh().loading = false;
        if let Some(l) = self.listen.as_mut().filter(|_| !paused) {
            l.go();
        }
        self.remember_wave(&t);
        if let Some(listen) = self.listen.as_mut().filter(|_| resume_position.is_some()) { listen.seek = true; }
        self.publish_soon();
        self.prefetch_next();
    }

    fn take_current_replacement(&mut self) -> Option<Audio> {
        let track = self.current.as_ref()?;
        let revision = crate::resolver_login::session_revision();
        self.current_check.as_mut()
            .filter(|c| replacements_enabled() && c.matches(track, revision))?.audio.take()
    }

    fn check_replacement(&mut self) {
        if !self.loaded || !replacements_enabled() { return; }
        let revision = crate::resolver_login::session_revision();
        let Some(track) = self.current.as_ref() else { return };
        // An account can connect after the first catalog buffer was already queued.
        // Run this cheap lookup independently of an old account's in-flight ASR.
        if self.current_lookup_revision != Some(revision) {
            self.current_lookup_revision = Some(revision);
            if let (Some(api), Some((baseline, _))) = (self.api.clone(), self.catalog_buf.clone()) {
                let track = track.clone();
                let owner = self.current_load.insert(Arc::new(())).clone();
                let job = Arc::downgrade(&owner);
                self.next_check = None;
                self.current_check = Some(ReplacementCheck { id: track.id.clone(), revision, owner, finished: false, audio: None });
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let current = || job.strong_count() > 0 && replacements_enabled() && crate::resolver_login::session_revision() == revision;
                    let audio = verified_known(&api, &track, &current, || true, || Some(baseline));
                    let _ = tx.send(Msg::KnownFinished { owner: job, revision, audio });
                });
                return;
            }
        }
        if !self.current_check.as_ref().is_some_and(|c| c.matches(track, revision)) {
            if self.paused { return; }
            let Some((buf, _)) = self.catalog_buf.clone() else { return };
            self.next_check = None;
            self.current_check = self.schedule_check(track.clone(), buf, revision);
            return;
        }
        if self.fading.is_none() {
            if let Some((buf, decoder, format)) = self.take_current_replacement() {
                let position = self.pos();
                let paused = self.paused;
                let src = self.source(decoder, Duration::ZERO);
                let success = if let Some(sink) = self.sink() {
                    sink.pause();
                    sink.clear();
                    sink.append(src);
                    let ok = sink.try_seek(Duration::from_secs_f32(position)).is_ok();
                    if !paused { sink.play(); }
                    ok
                } else { false };
                if success {
                    if let Some(track) = &self.current {
                        log(format!("Замена буфера {}: {format}, {:.2} с", track.id, position));
                    }
                    self.current_buf = Some(buf);
                    self.sh().format = format;
                    (self.notify)();
                } else {
                    if let Some(sink) = self.sink() { sink.clear(); }
                    let _ = self.restore_at(position);
                }
            }
        }
        // The playing track gets the single verification worker first, followed by the queue head.
        let next = self.queue.front();
        if self.next_check.as_ref().is_some_and(|c| next.is_none_or(|t| !c.matches(t, revision))) {
            self.next_check = None;
        }
        if self.paused {
            self.next_check.take_if(|c| !c.finished);
            return;
        }
        if self.next_check.is_some() || !self.current_check.as_ref().is_some_and(|c| c.finished) { return; }
        let (Some(next), Some((id, baseline))) = (next, &self.prefetch) else { return };
        if next.id == *id && baseline.known.revision() != Some(revision) {
            self.next_check = self.schedule_check(next.clone(), baseline.baseline.0.clone(), revision);
        }
    }

    fn schedule_check(&self, track: Track, buf: Arc<Buf>, revision: u64) -> Option<ReplacementCheck> {
        let api = self.api.clone()?;
        let check = ReplacementCheck {
            id: track.id.clone(), revision, owner: Arc::new(()), finished: false, audio: None,
        };
        let job = Arc::downgrade(&check.owner);
        let tx = self.tx.clone();
        *self.pending_replacement.lock().unwrap() = Some(Box::new(move || {
            let current = || job.strong_count() > 0 && replacements_enabled()
                && crate::resolver_login::session_revision() == revision;
            if !current() { return; }
            let credential = crate::resolver_login::credential();
            let retry = std::cell::Cell::new(false);
            let mut audio = resolve_track(&api, &track, buf.clone(), &current, &retry);
            if audio.is_none() && retry.get() && current() && credential.is_some() {
                // A busy source or interrupted connection gets another fresh set of one-use tickets.
                for _ in 0..10 {
                    if !current() { return; }
                    thread::sleep(Duration::from_millis(200));
                }
                audio = resolve_track(&api, &track, buf, &current, &retry);
            }
            if current() {
                let audio = audio.filter(|_| credential == crate::resolver_login::credential());
                let _ = tx.send(Msg::Replacement { job, audio });
            }
        }));
        let _ = self.replacement_wakeup.try_send(());
        Some(check)
    }

    fn publish_soon(&mut self) {
        self.ynison_due = Some(Instant::now() + Duration::from_secs(1));
    }

    /// Записать в Ynison, что играет: оригинал и следующий запуск продолжат отсюда.
    fn publish(&mut self) {
        self.ynison_due = None;
        self.ynison_at = Instant::now();
        let (Some(api), Some(now)) = (self.api.clone(), self.ynison_now()) else { return };
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = crate::ynison::publish(api.token(), &crate::ynison::device_id(), &now) {
                log(format!("ynison: {e}"));
            }
        }));
    }

    fn ynison_now(&self) -> Option<crate::ynison::Now> {
        let current = self.current.as_ref()?;
        let (entity_type, entity_id) = match &self.source {
            Source::Wave => ("RADIO", WAVE.to_string()),
            Source::Seed { station, .. } => ("RADIO", station.clone()),
            Source::List { kind: ListKind::Playlist, id, .. } => ("PLAYLIST", id.clone()),
            Source::List { kind: ListKind::Likes, .. } => ("PLAYLIST", format!("{}:3", self.uid)),
            Source::List { kind: ListKind::Album, id, .. } => ("ALBUM", id.clone()),
            Source::List { kind: ListKind::Artist, id, .. } => ("ARTIST", id.clone()),
            Source::List { kind: ListKind::Tracks, .. } => ("VARIOUS", String::new()),
        };
        // У списка — весь список, у волны — текущий трек и то, что уже подобрано дальше.
        let (tracks, index): (Vec<&Track>, usize) = match &self.list {
            Some(l) => match l.all.iter().position(|t| t.id == current.id) {
                Some(i) => (l.all.iter().collect(), i),
                None => (vec![current], 0),
            },
            None => (std::iter::once(current).chain(self.queue.iter().take(20)).collect(), 0),
        };
        let playable = |t: &&Track| crate::ynison::Playable {
            id: t.id.clone(),
            album_id: t.album_id.clone().unwrap_or_default(),
            title: t.title.clone(),
        };
        Some(crate::ynison::Now {
            entity_type,
            entity_id,
            from: self.play_context().from,
            tracks: tracks.iter().map(playable).collect(),
            index,
            progress_ms: (self.playback_position() * 1000.0) as u64,
            duration_ms: current.duration_ms,
            paused: self.paused,
            session: if self.list.is_none() { self.session.clone() } else { String::new() },
        })
    }

    /// Продолжить то, что играло последним в любом клиенте, с той же секунды и на паузе.
    fn apply_resume(&mut self, r: Resume) {
        let Some(track) = r.tracks.get(r.index).cloned() else { return };
        self.resume_at = Some((track.id.clone(), r.position));
        if r.source.station().is_some() {
            if r.source != self.source {
                self.set_source(r.source);
                self.feedback("radioStarted", None, None);
            }
            self.queue.push_back(track);
            self.play_from_queue();
            self.fetch_batch();
        } else {
            self.cmd(Cmd::PlayList { source: r.source, tracks: r.tracks, start: r.index, after: After::Loop });
        }
        self.sh().resumed = true;
    }

    // ---------- память волны ----------

    fn remember_wave(&mut self, t: &Track) {
        if self.source.station() == Some(self.played_station.as_str()) && !t.batch_id.is_empty() {
            self.save_wave_with(Some(t.full_id()));
        }
    }

    /// «Моя волна» после перезапуска продолжается, как у оригинала: сервер клонирует прошлую
    /// сессию вместе с её историей. Иначе волна начинает с чистого листа.
    fn restore_wave(&mut self) {
        let first = !std::mem::replace(&mut self.wave_restored, true);
        let Some(w) = load_wave_state() else { return };
        if w.uid != self.uid {
            return;
        }
        // Сессию берём только при запуске; если сервер её уже закрыл, wave_session начнёт новую.
        let fresh = crate::config::now_secs().saturating_sub(w.at) < WAVE_SESSION_TTL;
        if first && fresh && self.session.is_empty() && !w.session.is_empty() {
            self.session = w.session;
            self.clone_pending = true;
            self.restore_last = Some(w.last).filter(|l| !l.is_empty());
        }
    }

    fn save_wave(&self) {
        let last = self.current.as_ref().filter(|t| !t.batch_id.is_empty()).map(|t| t.full_id());
        self.save_wave_with(last);
    }

    fn save_wave_with(&self, last: Option<String>) {
        if self.played_station != WAVE || self.source.station() != Some(WAVE) || self.uid.is_empty() {
            return;
        }
        let w = serde_json::json!({
            "uid": self.uid,
            "session": self.session,
            "last": last.unwrap_or_default(),
            "at": crate::config::now_secs(),
        });
        let _ = self.bg.send(Box::new(move || {
            let path = crate::config::dir().join("wave.json");
            if let Err(e) = std::fs::write(&path, w.to_string()) {
                log(format!("wave.json: {e}"));
            }
        }));
    }

    fn on_load_error(&mut self, e: String) {
        self.errors_in_row += 1;
        let name = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
        self.error(format!("«{name}» не загрузился: {e}"));
        self.listen = None;
        if self.errors_in_row < 5 {
            self.current = None;
            self.stop_audio();
            self.play_from_queue();
        } else {
            self.sh().loading = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn seek_keeps_latest_position_until_audio_acknowledges_it() {
        let (tx, rx) = mpsc::channel();
        let mut e = Engine::new(tx.clone(), Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let player = Player { tx, shared: e.shared.clone(), likes: e.likes.clone(), eq: e.eq.clone() };
        e.loaded = true;
        e.held_pos = Some(10.0);
        e.sh().track = crate::api::parse_track(&serde_json::json!({"id": 1, "title": "seek"}));
        e.sh().duration = 100.0;

        for seconds in [80.0, 20.0] {
            assert_eq!(player.seek(seconds), Some(seconds));
            e.sync();
            assert_eq!(player.state().position, seconds);
        }
        e.handle(rx.recv().unwrap());
        e.sync();
        assert_eq!(e.held_pos, Some(10.0));
        assert_eq!(player.state().position, 20.0);
        e.handle(rx.recv().unwrap());
        e.sync();
        assert_eq!(e.held_pos, Some(20.0));
        e.held_pos = Some(21.0);
        e.sync();
        assert_eq!(player.state().position, 21.0);

        player.seek(90.0);
        e.gen += 1; // Перед обработкой команды начался другой трек.
        e.held_pos = Some(0.0);
        e.handle(rx.recv().unwrap());
        e.sync();
        assert_eq!(player.state().position, 0.0);
    }

    #[test]
    fn queued_likes_keep_the_last_saved_state_on_error() {
        let mut e = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let (bg, _jobs) = mpsc::channel();
        e.bg = bg;
        e.api = Some(Api::new(""));
        e.uid = "account".into();
        let track = crate::api::parse_track(&serde_json::json!({"id": 7, "title": "Track"})).unwrap();
        let reply = |liked, result| Msg::LikeFinished { uid: "account".into(), id: "7".into(), liked, result };
        e.toggle_like(track.clone(), None);
        e.toggle_like(track.clone(), None);
        e.handle(reply(true, Ok(())));
        assert!(!e.likes.lock().unwrap().contains("7"));
        e.handle(reply(false, Err("offline".into())));
        assert!(e.likes.lock().unwrap().contains("7"));
        e.toggle_like(track, None);
        e.handle(reply(false, Ok(())));
        e.handle(Msg::Likes("account".into(), Ok(vec!["7".into(), "9".into()])));
        let likes = e.likes.lock().unwrap();
        assert!(!likes.contains("7"));
        assert!(likes.contains("9"));
    }

    /// Сетевой тест: ссылка на поток → скачивание → декодирование (без токена — 30-сек превью).
    #[test]
    #[ignore]
    fn stream_and_decode() {
        let api = Api::new("");
        let (url, _) = api.stream_url("57703").expect("stream url");
        let bytes = api.fetch_bytes(&url).expect("download");
        assert!(bytes.len() > 50_000);
        let dec = Decoder::new(Cursor::new(bytes)).expect("decode");
        let samples = dec.take(44100).count();
        assert!(samples > 1000);
    }

    #[test]
    #[ignore = "requires a dedicated signed-in test profile and authorized catalog account"]
    fn resolve_live_catalog_recordings() {
        let profile = std::env::var_os("YM_TEST_PROFILE").expect("use an isolated test profile");
        assert_eq!(crate::config::dir(), std::path::PathBuf::from(profile));
        assert!(crate::resolver_login::credential().is_some());
        let api = Api::new(&std::env::var("YM_TOKEN").expect("local catalog credential"));
        for id in std::env::var("YM_TEST_TRACK_IDS").unwrap().split(',') {
            let t = api.tracks(&[id.to_owned()]).unwrap().pop().unwrap();
            let (buf, baseline, _) = load_audio(&api, &t.id).unwrap();
            let retry = std::cell::Cell::new(false);
            if std::env::var_os("YM_TEST_OFFLINE_SOURCES").is_some() {
                assert!(resolve_track(&api,&t,buf.clone(),&|| true,&retry).is_none());
                assert!(retry.get());
                assert!(baseline.take(44100).any(|sample| sample.unsigned_abs() > 32));
                eprintln!("original {id} decoded after source connection failed");
                continue;
            }
            let audio = resolve_track(&api, &t, buf.clone(), &|| true, &retry).or_else(|| {
                if !retry.get() { return None; }
                thread::sleep(Duration::from_secs(2));
                resolve_track(&api, &t, buf, &|| true, &retry)
            });
            let (_, _, format) = audio.unwrap_or_else(|| panic!("no verified replacement for {id}"));
            eprintln!("verified catalog {id}: {format}");
        }
    }

    fn make_audio(format: &str) -> Audio {
        // Three silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let bytes = frame.repeat(3);
        let response = tungstenite::http::Response::builder()
            .header("content-length", bytes.len()).body(bytes).unwrap();
        let buf = Buf::from_media(response.into(), &|| true).unwrap();
        let decoder = open_decoder(&buf).unwrap();
        (buf, decoder, format.to_owned())
    }

    #[test]
    fn first_decoder_uses_verified_audio_and_keeps_catalog_after_session_change() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        let track = crate::api::parse_track(&serde_json::json!({"id":7, "title":"Track"})).unwrap();
        let revision = crate::resolver_login::session_revision();
        let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        engine.current = Some(track.clone());
        engine.current_load = Some(Arc::new(()));
        for (issued, expected) in [(revision, "known"), (revision.wrapping_add(1), "catalog")] {
            let alternative = make_audio("provider");
            let baseline = make_audio("catalog");
            let catalog = baseline.0.clone();
            let expected_buf = if expected == "known" { alternative.0.clone() } else { baseline.0.clone() };
            let loaded = LoadedTrack { baseline, known: KnownStatus::Verified(issued, alternative) };
            let ((buf, mut decoder, _), origin) = engine.initial_audio(&track, loaded);
            assert_eq!(origin, expected);
            assert!(Arc::ptr_eq(&engine.catalog_buf.as_ref().unwrap().0, &catalog));
            assert!(Arc::ptr_eq(&buf, &expected_buf));
            assert!(decoder.next().is_some());
        }
    }

    #[test]
    fn restored_pause_holds_baseline_until_known_finishes_or_the_user_plays() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        let track = crate::api::parse_track(&serde_json::json!({"id":7, "title":"Track"})).unwrap();
        let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        engine.current = Some(track.clone());
        engine.current_load = Some(Arc::new(()));
        let revision = crate::resolver_login::session_revision();
        engine.resume_at = Some((track.id, 12.0));
        engine.play_loaded(LoadedTrack { baseline: make_audio("catalog"), known: KnownStatus::Pending(revision) });
        assert!(engine.paused_start.is_some());
        assert!(!engine.loaded);
        assert!(engine.audio.is_none());
        assert_eq!(engine.ynison_now().unwrap().progress_ms, 12_000);
        let stale = Arc::new(());
        engine.handle(Msg::KnownFinished { owner: Arc::downgrade(&stale), revision, audio: Some(make_audio("stale")) });
        assert!(engine.paused_start.is_some());
        assert_eq!(engine.resume_at.as_ref().map(|p| p.1), Some(12.0));
        engine.stop_audio();
        assert!(engine.paused_start.is_none());
    }

    #[test]
    fn late_known_audio_skips_asr_but_cannot_replace_a_different_track() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        let track = crate::api::parse_track(&serde_json::json!({"id":7, "title":"Track"})).unwrap();
        let revision = crate::resolver_login::session_revision();
        let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        engine.current = Some(track.clone());
        let owner = Arc::downgrade(engine.current_load.insert(Arc::new(())));
        let (events, pending) = mpsc::channel();
        events.send(KnownEvent::Found).unwrap();
        let known = initial_status(&pending, revision, Duration::from_millis(5));
        assert!(matches!(known, KnownStatus::Pending(_)));
        let loaded = LoadedTrack { baseline: make_audio("baseline"), known };
        assert_eq!(engine.initial_audio(&track, loaded).1, "catalog");
        assert!(!engine.current_check.as_ref().unwrap().finished);
        let stale = Arc::new(());
        engine.handle(Msg::KnownFinished { owner: Arc::downgrade(&stale), revision, audio: Some(make_audio("stale")) });
        assert!(engine.take_current_replacement().is_none());
        engine.handle(Msg::KnownFinished { owner, revision, audio: Some(make_audio("provider")) });
        assert!(engine.current_check.as_ref().unwrap().finished);
        assert_eq!(engine.take_current_replacement().unwrap().2, "provider");
    }

    #[test]
    #[ignore = "requires a dedicated signed-in test profile and approved server pair"]
    fn live_first_buffer() {
        let profile = std::env::var_os("YM_TEST_PROFILE").expect("use an isolated profile");
        assert_eq!(crate::config::dir(), std::path::PathBuf::from(profile));
        assert!(crate::resolver_login::credential().is_some());
        let api = Api::new(&std::env::var("YM_TOKEN").expect("local catalog credential"));
        let expected = std::env::var("YM_TEST_FIRST_SOURCE").expect("known or catalog");
        for id in std::env::var("YM_TEST_TRACK_IDS").unwrap().split(',') {
            let track = api.tracks(&[id.to_owned()]).unwrap().pop().unwrap();
            let (tx, rx) = mpsc::channel();
            let mut engine = Engine::new(tx.clone(), Default::default(), Default::default(), Arc::new(|| {}), Default::default());
            engine.current = Some(track.clone());
            let owner = Arc::downgrade(engine.current_load.insert(Arc::new(())));
            let started = Instant::now();
            let (catalog, t) = (api.clone(), track.clone());
            thread::spawn(move || load_for_playback(catalog, t, owner, tx.clone(), |res| {
                let _ = tx.send(Msg::Audio { gen: 0, res });
            }));
            let Msg::Audio { res, .. } = rx.recv_timeout(Duration::from_secs(60)).unwrap() else { panic!("missing first buffer") };
            let ((buf, decoder, format), origin) = engine.initial_audio(&track, res.unwrap());
            let elapsed = started.elapsed().as_millis();
            // The decoder is exactly the one returned to play_loaded for the first sink.append.
            assert!(decoder.take(6 * 96000).any(|sample| sample.unsigned_abs() > 32));
            eprintln!("first buffer {id} {origin} {format}, {elapsed}ms, encoded {}", crate::originals::media_digest(buf.reader()).unwrap());
            assert_eq!(origin, expected, "first decoder for {id}");
        }
    }

    #[test]
    fn new_session_checks_playing_catalog_once_before_scheduling_asr() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        let revision = crate::resolver_login::session_revision();
        let (tx, rx) = mpsc::channel();
        let mut e = Engine::new(tx, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let (wakeup, _jobs) = mpsc::sync_channel(1);
        e.replacement_wakeup = wakeup;
        // No artist metadata: the fast miss is deterministic and never makes a network request.
        e.current = crate::api::parse_track(&serde_json::json!({"id":7, "title":"Track"}));
        e.api = Some(Api::new(""));
        e.loaded = true;
        e.paused = false;
        e.current_lookup_revision = Some(revision.wrapping_sub(1));
        e.catalog_buf = Some((make_audio("catalog").0, "catalog".into()));
        e.check_replacement();
        let miss = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(miss, Msg::KnownFinished { revision: r, audio: None, .. } if r == revision));
        assert!(e.pending_replacement.lock().unwrap().is_none());
        e.handle(miss);
        e.check_replacement();
        assert!(e.pending_replacement.lock().unwrap().is_some());
        assert_eq!(e.current_lookup_revision, Some(revision));
    }

    #[test]
    fn ordinary_mode_restores_catalog_buffer_and_preserves_paused_position_and_queue() {
        let track = |id| crate::api::parse_track(&serde_json::json!({"id":id,"title":"Track"})).unwrap();
        let mut e = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let catalog = make_audio("MP3 192").0;
        e.catalog_buf = Some((catalog.clone(), "MP3 192".into()));
        e.current_buf = Some(make_audio("provider").0);
        e.current = Some(track(7));
        e.queue.push_back(track(8));
        e.loaded = true;
        e.paused = true;
        e.held_pos = Some(12.0);
        e.sources_mode_changed(false);
        assert!(Arc::ptr_eq(&catalog, e.current_buf.as_ref().unwrap()));
        assert_eq!(e.sh().format, "MP3 192");
        assert!(e.paused);
        assert_eq!(e.pos(), 12.0);
        assert_eq!(e.current.as_ref().unwrap().id, "7");
        assert_eq!(e.queue.front().unwrap().id, "8");
    }

    #[test]
    fn wave_and_list_promote_only_the_matching_check_and_consume_ready_audio_once() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        for list in [false, true] { check_queue_promotion(list); }
    }

    fn check_queue_promotion(list: bool) {
        let track = |id| crate::api::parse_track(&serde_json::json!({"id":id,"title":"Track"})).unwrap();
        let mut e = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        // Drive completions ourselves, without network requests or an audio device.
        let (wakeup, _rx) = mpsc::sync_channel(1);
        e.replacement_wakeup = wakeup;
        e.api = Some(Api::new(""));
        e.current = Some(track(1));
        if list { e.list = Some(ListPlay { all: vec![track(2)], deferred: VecDeque::new(), after: After::Loop }); }
        e.loaded = true;
        e.paused = false;
        e.current_lookup_revision = Some(crate::resolver_login::session_revision());
        e.catalog_buf = Some((make_audio("original").0, "catalog".into()));
        e.queue.push_back(track(2));
        e.prefetch = Some(("2".into(), LoadedTrack { baseline: make_audio("baseline"), known: KnownStatus::Miss(crate::resolver_login::session_revision()) }));
        e.check_replacement();
        assert!(e.next_check.is_none());
        let active = Arc::downgrade(&e.current_check.as_ref().unwrap().owner);
        e.handle(Msg::Replacement { job: active, audio: None });
        e.check_replacement();
        let suspended = Arc::downgrade(&e.next_check.as_ref().unwrap().owner);
        e.paused = true;
        e.check_replacement();
        assert_eq!(suspended.strong_count(), 0);
        e.paused = false;
        e.check_replacement();
        let discarded = Arc::downgrade(&e.next_check.as_ref().unwrap().owner);
        e.queue.push_front(track(3));
        e.prefetch = Some(("3".into(), LoadedTrack { baseline: make_audio("baseline"), known: KnownStatus::Miss(crate::resolver_login::session_revision()) }));
        e.check_replacement();
        e.handle(Msg::Replacement { job: discarded, audio: Some(make_audio("stale")) });
        assert!(!e.next_check.as_ref().unwrap().finished);
        let prepared = Arc::downgrade(&e.next_check.as_ref().unwrap().owner);
        e.handle(Msg::Replacement { job: prepared, audio: Some(make_audio("verified")) });
        assert!(e.take_current_replacement().is_none());
        e.prefetch = None;
        e.api = None;
        e.play_from_queue();
        assert_eq!(e.current.as_ref().unwrap().id, "3");
        assert_eq!(e.take_current_replacement().unwrap().2, "verified");
        assert!(e.take_current_replacement().is_none());
    }

    #[test]
    fn in_flight_wave_check_survives_promotion_but_not_account_reset() {
        let _guard = super::EXPLICIT_TEST_LOCK.lock().unwrap();
        let mut e = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let next = crate::api::parse_track(&serde_json::json!({"id":17,"title":"Next"})).unwrap();
        let owner = Arc::new(());
        let job = Arc::downgrade(&owner);
        e.next_check = Some(ReplacementCheck {
            id: next.id.clone(), revision: crate::resolver_login::session_revision(), owner, finished: false, audio: None,
        });
        e.queue.push_back(next);
        e.play_from_queue();
        e.handle(Msg::Replacement { job: job.clone(), audio: None });
        assert!(e.current_check.as_ref().unwrap().finished);
        e.reset();
        assert_eq!(job.strong_count(), 0);
        e.handle(Msg::Replacement { job, audio: None });
        assert!(e.current_check.is_none());
    }

    #[test]
    fn rejects_prefetch_from_before_account_reset_even_for_the_same_track() {
        let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
        let old_gen = engine.prefetch_gen;
        let track_id = "123".to_owned();
        engine.prefetching = Some(track_id.clone());
        engine.reset();
        engine.prefetching = Some(track_id.clone());
        engine.handle(Msg::Prefetch { gen: old_gen, id: track_id.clone(), res: Err("old account".into()) });
        assert_eq!(engine.prefetching, Some(track_id));
    }
}

#[cfg(test)]
static EXPLICIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[test]
fn explicit_filter_survives_repeat_and_previous() {
    let _guard = EXPLICIT_TEST_LOCK.lock().unwrap();
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default());
    let blocked = crate::api::parse_track(&serde_json::json!({"id":1,"title":"E","explicit":true})).unwrap();
    let allowed = crate::api::parse_track(&serde_json::json!({"id":2,"title":"A"})).unwrap();
    let previous = crate::api::ALLOW_EXPLICIT.swap(false, std::sync::atomic::Ordering::Relaxed);
    engine.list = Some(ListPlay { deferred: VecDeque::new(), all: vec![blocked.clone(), allowed.clone()], after: After::Loop });
    engine.repeat = Repeat::All;
    engine.play_from_queue();
    let repeated = engine.current.take().map(|t| t.id);
    engine.history = vec![allowed, blocked];
    engine.prev();
    let previous_track = engine.current.take().unwrap().id;
    let explicit = engine.list.as_ref().unwrap().all[0].clone();
    engine.list.as_mut().unwrap().all = vec![explicit.clone()];
    engine.list.as_mut().unwrap().after = After::Wave(Source::Wave);
    engine.repeat = Repeat::Off;
    engine.queue = VecDeque::from([explicit]);
    engine.play_from_queue();
    engine.set_allow_explicit(true);
    engine.cmd(Cmd::PlayPause);
    let resumed = engine.current.take().map(|t| t.id);
    // Список только из 18+ при выключенном фильтре не включается и не трогает текущий трек.
    crate::api::ALLOW_EXPLICIT.store(false, std::sync::atomic::Ordering::Relaxed);
    let kept = engine.list.as_ref().unwrap().all[0].clone();
    let mut other = kept.clone();
    other.id = "3".into();
    engine.current = Some(kept.clone());
    engine.cmd(Cmd::PlayList { source: Source::Wave, tracks: vec![other, kept], start: 1, after: After::Loop });
    let still = engine.current.as_ref().map(|t| t.id.clone());
    let told = engine.sh().error.is_some();
    crate::api::ALLOW_EXPLICIT.store(previous, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(still.as_deref(), Some("1"));
    assert!(told);
    assert_eq!(resumed.as_deref(), Some("1"));
    assert_eq!(repeated.as_deref(), Some("2"));
    assert_eq!(previous_track, "2");
}
