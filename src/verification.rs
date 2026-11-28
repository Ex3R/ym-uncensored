//! Verify the audible change before handing a replacement to the mixer.
use crate::api::{Lyrics, OmissionCheck, OmissionPlan};
use std::{cell::RefCell, collections::HashMap};
use rodio::{source::UniformSourceIterator, Source};
use whisper_rs::{FullParams, SamplingStrategy};

#[path = "remote_speech.rs"]
pub(crate) mod remote_speech;

const RATE: usize = 16_000;

const MAX_SECONDS: usize = 12 * 60;

pub(crate) struct Recording {
    pcm: Vec<i16>,
    envelope: Vec<f32>,
    mono_splices: Vec<(u32, u32)>,
    reference_transcripts: RefCell<HashMap<(u32, u32), Vec<String>>>,
    speech_transcripts: RefCell<HashMap<(usize, usize, bool), Vec<String>>>,
    pub digest: [u8; 32],
}

impl Recording {
    pub(crate) fn with_speech<T>(&self, provider: Option<std::sync::Arc<remote_speech::Provider>>, run: impl FnOnce() -> T) -> T {
        if let Some(provider) = provider { remote_speech::with_candidate(&self.pcm, provider, run) } else { run() }
    }
    pub fn decode(source: impl Source<Item = i16>, duration_ms: u64) -> Option<Self> {
        use sha2::{Digest, Sha256};
        if duration_ms > MAX_SECONDS as u64 * 1000 {
            return None;
        }
        let limit = (duration_ms as usize / 1000 + 4) * RATE;
        // Gapless MP3 can begin with a completely trimmed packet. Rodio 0.20's
        // UniformSourceIterator takes zero samples from it and never advances the decoder.
        let (pcm, mono_splices) = if source.current_frame_len() == Some(0) {
            decode_samples(rodio::source::from_iter([source]), limit)
        } else {
            decode_samples(source, limit)
        };
        let decoded_ms = pcm.len() / 16;
        let shorter_padding = duration_ms >= 60_000
            && decoded_ms < duration_ms as usize
            && duration_ms as usize - decoded_ms <= 5200;
        if (!shorter_padding && decoded_ms.abs_diff(duration_ms as usize) > 2200) || pcm.len() < 4 * RATE {
            return None;
        }
        let mut hash = Sha256::new();
        let mut envelope = Vec::with_capacity(pcm.len() / 16);
        let mut previous = 0.0;
        for chunk in pcm.chunks(2048) {
            let mut bytes = [0_u8; 4096];
            for (dst, sample) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(chunk) {
                dst.copy_from_slice(&sample.to_le_bytes());
            }
            hash.update(&bytes[..chunk.len() * 2]);
            for frame in chunk.as_chunks::<16>().0 {
                let value = frame.iter().map(|&s| s as f32).sum::<f32>() / (16.0 * 32768.0);
                envelope.push(value - previous);
                previous = value;
            }
        }
        Some(Self {
            pcm,
            envelope,
            mono_splices,
            reference_transcripts: RefCell::default(), speech_transcripts: RefCell::default(),
            digest: hash.finalize().into(),
        })
    }
}

// Capture stereo width before folding to mono for speech recognition. Center
// removal can leave an audible backing track in both channels, with no silence
// or tone to distinguish the edit in the downmix.
fn decode_samples(source: impl Source<Item = i16>, limit: usize) -> (Vec<i16>, Vec<(u32, u32)>) {
    let mut stereo = UniformSourceIterator::<_, i16>::new(source, 2, RATE as u32);
    let mut width = Vec::new();
    let (mut mid_power, mut side_power) = (0_i64, 0_i64);
    let mut frames = 0;
    let pcm = std::iter::from_fn(|| {
        let (left, right) = (i32::from(stereo.next()?), i32::from(stereo.next()?));
        let mid = (left + right) / 2;
        let side = (left - right) / 2;
        mid_power += i64::from(mid).pow(2);
        side_power += i64::from(side).pow(2);
        frames += 1;
        if frames == RATE / 100 {
            let audible = mid_power >= 42_746 * frames as i64; // -44 dBFS
            let narrow = audible && side_power * 100_000 <= mid_power; // -50 dB relative to mid
            let wide = audible && side_power * 1000 >= mid_power; // at least -30 dB
            width.push((narrow, wide));
            (mid_power, side_power, frames) = (0, 0, 0);
        }
        Some(mid as i16)
    }).take(limit).collect();
    (pcm, mono_splices(&width))
}

fn mono_splices(frames: &[(bool, bool)]) -> Vec<(u32, u32)> {
    let mut splices = Vec::new();
    let mut i = 0;
    while i < frames.len() {
        let start = i;
        if !frames[i].0 { i += 1; continue; }
        while i < frames.len() && frames[i].0 { i += 1; }
        if !(18..=170).contains(&(i - start)) || start < 12 || i + 12 > frames.len() { continue; }
        let wide = |flank: &[(bool, bool)]| flank.iter().filter(|(_, wide)| *wide).count() >= 6;
        if wide(&frames[start - 12..start]) && wide(&frames[i..i + 12]) {
            splices.push((start as u32 * 10, i as u32 * 10));
        }
    }
    splices
}

// A mono passage is common in music. Only treat it as a remaining edit when
// the same kind of splice was removed at the independently verified word.
fn has_remaining_mono_splice(original: &Recording, candidate: &Recording, lag: isize, restored_ms: u32) -> bool {
    let retained = |&(start, end): &(u32, u32)| {
        let shifted = |t: u32| (t as usize).saturating_add_signed(lag) as u32;
        candidate.mono_splices.iter().any(|&(a, b)|
            a.abs_diff(shifted(start)) <= 40 && b.abs_diff(shifted(end)) <= 40)
    };
    let near_word = |&(start, end): &(u32, u32)| {
        restored_ms.abs_diff(restored_ms.clamp(start, end)) <= 280
    };
    original.mono_splices.iter().any(|mark| near_word(mark) && !retained(mark))
        && original.mono_splices.iter().any(|mark| !near_word(mark) && retained(mark))
}

// A shorter candidate may only omit padding, never an audible ending. Check
// every 10 ms of the unmatched tail after alignment, allowing codec noise at -60 dBFS.
fn silent_extra_tail(original: &[i16], candidate: &[i16], lag: isize) -> bool {
    if original.len().abs_diff(candidate.len()) <= 80 * (RATE / 1000) { return true; }
    let start_a = (-lag).max(0) as usize * (RATE / 1000);
    let start_b = lag.max(0) as usize * (RATE / 1000);
    let Some(a) = original.get(start_a..) else { return false; };
    let Some(b) = candidate.get(start_b..) else { return false; };
    let overlap = a.len().min(b.len());
    let quiet = |tail: &[i16]| tail.chunks(RATE / 100).all(|chunk|
        chunk.iter().map(|&v| i64::from(v).pow(2)).sum::<i64>() <= 33 * 33 * chunk.len() as i64);
    quiet(&a[overlap..]) && quiet(&b[overlap..])
}

fn correlation(a: &[f32], b: &[f32], step: usize) -> f32 {
    let (mut ab, mut aa, mut bb) = (0., 0., 0.);
    for i in (0..a.len()).step_by(step) {
        ab += a[i] * b[i];
        aa += a[i] * a[i];
        bb += b[i] * b[i];
    }
    if aa < 1e-8 && bb < 1e-8 {
        return 1.;
    }
    ab / (aa * bb).sqrt().max(1e-8)
}

fn alignment(a: &[f32], b: &[f32]) -> Option<isize> {
    let end = a.len().min(b.len());
    if end < 4500 {
        return None;
    }
    let mut votes = Vec::new();
    for i in 1..=7 {
        let pos = 2000 + (end - 4500) * i / 8;
        let anchor = &a[pos..pos + 400];
        if anchor.iter().map(|x| x * x).sum::<f32>() < 0.005 {
            continue;
        }
        let (score, lag) = (-2000_isize..=2000)
            .map(|lag| {
                let from = pos.checked_add_signed(lag).unwrap();
                (correlation(anchor, &b[from..from + 400], 3), lag)
            })
            .max_by(|x, y| x.0.total_cmp(&y.0))?;
        if score > 0.50 {
            votes.push(lag);
        }
    }
    votes.sort_unstable();
    let median = *votes.get(votes.len() / 2)?;
    (votes
        .iter()
        .filter(|&&lag| lag.abs_diff(median) <= 3)
        .count()
        >= 3)
        .then_some(median)
}

fn words(text: &str) -> Vec<String> {
    // Lyrics use digits where ASR spells a number. Keep decimals, fractions and
    // embedded identifiers untouched; only stand-alone small integers are expanded.
    const SMALL: [&str; 20] = ["ноль", "один", "два", "три", "четыре", "пять", "шесть", "семь", "восемь", "девять",
        "десять", "одиннадцать", "двенадцать", "тринадцать", "четырнадцать", "пятнадцать", "шестнадцать", "семнадцать", "восемнадцать", "девятнадцать"];
    const TENS: [&str; 10] = ["", "", "двадцать", "тридцать", "сорок", "пятьдесят", "шестьдесят", "семьдесят", "восемьдесят", "девяносто"];
    let expanded = text.split_whitespace().map(|token| {
        let core = token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '"' | '«' | '»' | ',' | '.' | '!' | '?' | ';'));
        if core.is_empty() || core.len() > 2 || !core.bytes().all(|c| c.is_ascii_digit())
            || (core.len() > 1 && core.starts_with('0')) { return token.to_owned(); }
        let number = core.parse::<usize>().unwrap();
        if number < 20 { SMALL[number].to_owned() }
        else if number % 10 == 0 { TENS[number / 10].to_owned() }
        else { format!("{} {}", TENS[number / 10], SMALL[number % 10]) }
    }).collect::<Vec<_>>().join(" ");
    expanded.to_lowercase()
        .replace('ё', "е")
        .split(|c: char| !c.is_alphabetic())
        .filter(|s| s.chars().count() >= 3)
        .map(str::to_owned)
        .collect()
}

fn full_speech_model() -> Option<&'static std::sync::Mutex<whisper_rs::WhisperContext>> {
    use std::sync::{Mutex, OnceLock};
    use whisper_rs::{WhisperContext, WhisperContextParameters};
    static SPEECH_MODEL: OnceLock<Option<Mutex<WhisperContext>>> = OnceLock::new();
    SPEECH_MODEL.get_or_init(|| {
        whisper_rs::install_logging_hooks();
        let path = crate::config::dir().join("ggml-small-q5_1.bin");
        #[cfg(test)]
        let path = std::env::var_os("YM_VERIFICATION_MODEL").map(std::path::PathBuf::from).unwrap_or(path);
        if !path.is_file() {
            #[cfg(target_os = "android")]
            return crate::sys::speech_model().map(Mutex::new);
            #[cfg(target_os = "windows")]
            {
                #[cfg(ym_windows_speech_model)]
                return WhisperContext::new_from_buffer_with_params(include_bytes!(env!("YM_WINDOWS_SPEECH_MODEL")), Default::default()).ok().map(Mutex::new);
                #[cfg(not(ym_windows_speech_model))]
                return None;
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let exe = std::env::current_exe().ok()?;
                #[cfg(target_os = "linux")]
                let resource = "../share/yamusic-rs/ggml-small-q5_1.bin";
                #[cfg(target_os = "macos")]
                let resource = "../Resources/ggml-small-q5_1.bin";
                let model = exe.parent()?.join(resource);
                return WhisperContext::new_with_params(model, Default::default()).ok().map(Mutex::new);
            }
            #[cfg(not(any(target_os="macos",target_os="linux",target_os="android",target_os="windows")))]
            return None;
        }
        WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .ok()
            .map(Mutex::new)
    }).as_ref()
}

fn transcript(pcm: &[i16], state: &mut whisper_rs::WhisperState) -> Option<Vec<String>> {
    if let Some(result) = remote_speech::transcript(pcm) { return Some(words(&result.text)); }
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("ru"));
    params.set_n_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as i32);
    params.set_no_context(true);
    params.set_temperature_inc(0.);
    params.set_max_tokens(48);
    // Short paired clips do not need the default 30-second encoder window.
    // Keep padding beyond the clip and leave longer inputs at the model default.
    if pcm.len() <= 8 * RATE { params.set_audio_ctx(512); }
    params.set_print_progress(false);
    params.set_print_timestamps(false);
    params.set_print_realtime(false);
    let samples: Vec<_> = pcm.iter().map(|&s| s as f32 / 32768.).collect();
    state.full(params, &samples).ok()?;
    let mut text = Vec::new();
    for segment in state.as_iter().filter(|s| s.no_speech_probability() < 0.55) {
        text.extend(words(segment.to_str().ok()?));
    }
    Some(text)
}

// Omission checks use the full model and retain short words, including prepositions
// and pronouns. Each result is local to a single time-aligned reference clip.
#[derive(Clone, Debug)]
struct TimedWord {
    word: String,
    from_ms: u32,
    to_ms: u32,
}

fn transcript_timed_words(pcm: &[i16], state: &mut whisper_rs::WhisperState) -> Option<Vec<TimedWord>> {
    if let Some(result) = remote_speech::transcript(pcm) {
        return Some(result.words.iter().filter_map(|word| {
            let normalized = words(&word.text);
            (normalized.len() == 1).then(|| TimedWord { word: normalized[0].clone(),
                from_ms: (word.start*1000.).round() as u32, to_ms: (word.end*1000.).round() as u32 })
        }).collect());
    }
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("ru"));
    params.set_n_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as i32);
    params.set_no_context(true);
    params.set_temperature_inc(0.);
    params.set_max_tokens(64);
    // Short paired clips do not need the default 30-second encoder window.
    // Keep padding beyond the clip and leave longer inputs at the model default.
    if pcm.len() <= 8 * RATE { params.set_audio_ctx(512); }
    params.set_token_timestamps(true);
    params.set_print_progress(false);
    params.set_print_timestamps(false);
    params.set_print_realtime(false);
    params.set_suppress_blank(true);
    let samples: Vec<_> = pcm.iter().map(|&s| s as f32 / 32768.).collect();
    state.full(params, &samples).ok()?;
    let mut result = Vec::new();
    for segment in state.as_iter().filter(|s| s.no_speech_probability() < 0.55) {
        let mut word = String::new();
        let (mut from_ms, mut to_ms) = (0, 0);
        let flush = |word: &mut String, from_ms: u32, to_ms: u32, result: &mut Vec<TimedWord>| {
            if word.chars().count() >= 3 {
                result.push(TimedWord { word: std::mem::take(word), from_ms, to_ms });
            } else { word.clear(); }
        };
        for i in 0..segment.n_tokens() {
            let token = segment.get_token(i)?;
            let text = token.to_str().ok()?.to_lowercase().replace('ё', "е");
            let data = token.token_data();
            let token_from = u32::try_from(data.t0).ok()?.saturating_mul(10);
            let token_to = u32::try_from(data.t1).ok()?.saturating_mul(10);
            if text.chars().next().is_some_and(char::is_whitespace) && !word.is_empty() {
                flush(&mut word, from_ms, to_ms, &mut result);
            }
            for c in text.chars() {
                if c.is_alphabetic() {
                    if word.is_empty() { from_ms = token_from; }
                    word.push(c);
                    to_ms = token_to;
                } else if !word.is_empty() {
                    flush(&mut word, from_ms, to_ms, &mut result);
                }
            }
        }
        if !word.is_empty() { flush(&mut word, from_ms, to_ms, &mut result); }
    }
    Some(result)
}

fn omission_transcript(pcm: &[i16], state: &mut whisper_rs::WhisperState, audio_ctx: i32) -> Option<Vec<String>> {
    if let Some(result) = remote_speech::transcript(pcm) {
        return Some(result.text.to_lowercase().replace('ё', "е").split(|c: char| !c.is_alphabetic())
            .filter(|word| !word.is_empty()).map(str::to_owned).collect());
    }
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
    params.set_language(Some("ru"));
    params.set_no_context(true);
    params.set_n_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as i32);
    params.set_audio_ctx(audio_ctx);
    params.set_no_timestamps(true);
    params.set_print_progress(false);
    params.set_print_timestamps(false);
    params.set_print_realtime(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    let samples: Vec<_> = pcm.iter().map(|&s| s as f32 / 32768.).collect();
    state.full(params, &samples).ok()?;
    let mut words = Vec::new();
    for segment in state.as_iter() {
        let text = segment.to_str().ok()?.to_lowercase().replace('ё', "е");
        words.extend(text.split(|c: char| !c.is_alphabetic()).filter(|s| !s.is_empty()).map(str::to_owned));
    }
    Some(words)
}

fn supports_omission(words: &[String], check: &OmissionCheck) -> bool {
    !words.contains(&check.missing) && words.windows(4).any(|anchors|
        anchors[..2] == check.before && anchors[2..] == check.after)
}

impl Recording {
    // Reuse only local recognition of this immutable recording's exact slice.
    // Tiny and full models are independent evidence; candidate-only assistance
    // remains scoped to its provider and must never be hidden by this cache.
    fn cached_words(&self, pcm: &[i16], full_model: bool, recognize: impl FnOnce() -> Option<Vec<String>>) -> Option<Vec<String>> {
        if remote_speech::owns(pcm) { return recognize(); }
        let Some(from) = (pcm.as_ptr() as usize).checked_sub(self.pcm.as_ptr() as usize)
            .map(|bytes| bytes / std::mem::size_of::<i16>()) else { return recognize(); };
        if pcm.is_empty() || from.checked_add(pcm.len()).is_none_or(|to| to > self.pcm.len()) {
            return recognize();
        }
        let key = (from, pcm.len(), full_model);
        if let Some(words) = self.speech_transcripts.borrow().get(&key) { return Some(words.clone()); }
        let words = recognize()?;
        let mut cache = self.speech_transcripts.borrow_mut();
        if cache.len() >= 256 { cache.clear(); }
        cache.insert(key, words.clone());
        Some(words)
    }

    fn cue(&self, key: (u32, u32), lag: isize) -> Option<&[i16]> {
        let index = |ms: u32| (ms as usize).saturating_add_signed(lag).saturating_mul(16).min(self.pcm.len());
        let samples = &self.pcm[index(key.0)..index(key.1)];
        (!samples.is_empty()).then_some(samples)
    }

    fn baseline_words(&self, key: (u32, u32), state: &mut whisper_rs::WhisperState) -> Option<Vec<String>> {
        let mut cache = self.reference_transcripts.borrow_mut();
        let words = match cache.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) =>
                entry.insert(omission_transcript(self.cue(key, 0)?, state, 512)?),
        };
        Some(words.clone())
    }
}

// Hard edits in the entire mix have a near-digital floor and audible flanks.
// The ranges are clip-relative milliseconds. A pause alone says nothing about a lyric.
fn hard_pauses(pcm: &[i16]) -> Vec<(u32, u32)> {
    let power: Vec<u64> = pcm.as_chunks::<160>().0.iter().map(|frame| frame.iter().map(|&s| {
        let x = i64::from(s);
        (x * x) as u64
    }).sum::<u64>() / 160).collect();
    let mut pauses = Vec::new();
    let mut i = 0;
    while i < power.len() {
        if power[i] > 33 { i += 1; continue; } // -75 dBFS
        let first = i;
        while i < power.len() && power[i] <= 33 { i += 1; }
        if !(18..=170).contains(&(i - first)) || first < 10 || i + 10 > power.len() { continue; }
        let audible = |side: &[u64]| side.iter().filter(|&&p| p >= 427_464).count() >= 5; // -34 dBFS
        if audible(&power[first-10..first-1]) && audible(&power[i+1..i+10]) {
            pauses.push((first as u32 * 10, i as u32 * 10));
        }
    }
    pauses
}

#[derive(Clone, Copy)]
struct Interruption {
    from: u32,
    to: u32,
    tone_hz: Option<f64>,
}

impl Interruption {
    fn matches(self, other: &Self) -> bool {
        self.from.abs_diff(other.from) <= 40 && self.to.abs_diff(other.to) <= 40 &&
            match (self.tone_hz, other.tone_hz) {
                (None, None) => true,
                (Some(a), Some(b)) => (a - b).abs() <= (a * 0.015).max(12.),
                _ => false,
            }
    }
}

// Estimate frequency from rising zero crossings, then test how much of the
// frame's energy a single sinusoid at that frequency actually accounts for.
fn pure_tone_hz(frame: &[i16]) -> Option<f64> {
    let mut first = 0.;
    let mut last = 0.;
    let mut crossings = 0;
    let mut power = 0.;
    for (index, pair) in frame.windows(2).enumerate() {
        let (a, b) = (f64::from(pair[0]), f64::from(pair[1]));
        power += a * a;
        if a <= 0. && b > 0. {
            last = index as f64 - a / (b - a);
            if crossings == 0 { first = last; }
            crossings += 1;
        }
    }
    power += f64::from(*frame.last()?).powi(2);
    if crossings < 5 || power < 65_536. * frame.len() as f64 { return None; }
    let hz = (crossings - 1) as f64 * RATE as f64 / (last - first);
    if !(400.0..=3200.0).contains(&hz) { return None; }
    let (step_sin, step_cos) = (std::f64::consts::TAU * hz / RATE as f64).sin_cos();
    let (mut sin, mut cos) = (0., 1.);
    let (mut ss, mut cc, mut sc) = (0., 0., 0.);
    let (mut xs, mut xc) = (0., 0.);
    for &sample in frame {
        let x = f64::from(sample);
        ss += sin * sin;
        cc += cos * cos;
        sc += sin * cos;
        xs += x * sin;
        xc += x * cos;
        (sin, cos) = (sin * step_cos + cos * step_sin, cos * step_cos - sin * step_sin);
    }
    let fitted_power = (cc * xs * xs - 2. * sc * xs * xc + ss * xc * xc) / (ss * cc - sc * sc);
    (fitted_power >= 0.975 * power).then_some(hz)
}

fn interruptions(pcm: &[i16]) -> Vec<Interruption> {
    let mut found: Vec<_> = hard_pauses(pcm).into_iter().map(|(from, to)|
        Interruption { from, to, tone_hz: None }).collect();
    let pitch: Vec<_> = pcm.windows(320).step_by(160).map(pure_tone_hz).collect();
    let mut i = 0;
    while i < pitch.len() {
        let first = i;
        i += 1;
        let Some(hz) = pitch[first] else { continue };
        let mut last = first;
        let mut good = 1;
        // Packet-boundary resampling can disturb up to two successive 10 ms hops.
        while i < pitch.len() && i - last <= 3 {
            if let Some(next) = pitch[i] {
                if (next - hz).abs() > (hz * 0.015).max(12.) { break; }
                last = i;
                good += 1;
            }
            i += 1;
        }
        let from = first as u32 * 10;
        let to = last as u32 * 10 + 20;
        if (180..=1700).contains(&(to - from)) && good * 3 >= 2 * (last - first + 1)
            && first >= 5 && last + 5 < pitch.len() {
            found.push(Interruption { from, to, tone_hz: Some(hz) });
        }
    }
    found
}

// When several positions share a time window, a neighboring word can survive a mask.
// Require each occurrence, in reference order and with a spoken immediate neighbor,
// before excusing the interruption. A single-position cue keeps its original test.
fn heard_cue_targets(spoken: &[String], targets: &[&OmissionCheck]) -> bool {
    if let [only] = targets { return spoken.contains(&only.missing); }
    let mut from = 0;
    for target in targets {
        let Some(at) = (from..spoken.len()).find(|&i| {
            spoken[i] == target.missing &&
                ((i > 0 && spoken[i - 1] == target.before[1]) || spoken.get(i + 1) == Some(&target.after[0]))
        }) else { return false };
        from = at + 1;
    }
    !targets.is_empty()
}

// Silence and narrow-band tones cost no speech recognition. Keep the absolute
// position so overlapping lyric windows cannot turn one edit into two.
fn shared_interruptions(original: &[i16], candidate: &[i16], key: (u32, u32),
    checks: &[OmissionCheck], lyrics: &Lyrics) -> Vec<Interruption> {
    let baseline = interruptions(original);
    if baseline.is_empty() { return Vec::new(); }
    interruptions(candidate).into_iter().filter_map(|mut mark| {
        if !baseline.iter().any(|other| mark.matches(other)) { return None; }
        mark.from += key.0;
        mark.to += key.0;
        let middle = (mark.from + mark.to) as f32 / 2000.;
        let line = lyrics.synced.iter().find(|line| line.from <= middle && line.to.is_some_and(|t| middle < t))?;
        let line_words = words(&line.text);
        checks.iter().any(|check| line_words.contains(&check.missing)).then_some(mark)
    }).collect()
}

// Keep aligned audio, lyric context and cancellation explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn review_shared_interruption(candidate: &Recording, key: (u32, u32),
    lag: isize, checks: &[OmissionCheck], lyrics: &Lyrics, state: &mut whisper_rs::WhisperState,
    mark: Interruption, current: &impl Fn() -> bool) -> Option<()> {
    let (start, end) = (mark.from, mark.to);
    let middle = (start + end) as f32 / 2000.;
    let line = lyrics.synced.iter().find(|line| line.from <= middle && line.to.is_some_and(|t| middle < t))?;
    let line_words = words(&line.text);
    let targets: Vec<_> = checks.iter().filter(|check| line_words.contains(&check.missing)).collect();
    // Close the apparent gap for recognition only. An intentional rest preserves
    // the word on one side. Extending the end keeps the amount of audible context.
    let mut heard = false;
    let extended_end = key.1 + end - start;
    let windows = [
        (key.0, extended_end),
        (key.0.saturating_sub(480), extended_end + 1030),
        (start.saturating_sub(2800), end + 2800),
    ];
    // A low-rate encode may only reveal the preserved word when the clip is centered on the tone.
    let count = if mark.tone_hz.is_some() { 3 } else { 2 };
    for &(left, right) in &windows[..count] {
        if !current() { return None; }
        let mut clip = candidate.cue((left, start), lag)?.to_vec();
        clip.extend_from_slice(candidate.cue((end, right), lag)?);
        let spoken = omission_transcript(&clip, state, 768)?;
        if heard_cue_targets(&spoken, &targets) { heard = true; break; }
    }
    if !heard { return None; }
    Some(())
}

// Prefer a word-length passage that still sounds like the baseline while the
// surrounding mix changed. Gain-invariant waveform agreement is only a ranking
// signal. It never proves that an expected word was spoken.
fn retained_passage_score(baseline: &[i16], candidate: &[i16]) -> f32 {
    const FRAME: usize = RATE / 100;
    const RUN: usize = 45;
    // Alignment votes are in whole milliseconds. Refine the phase inside the
    // cue before comparing adjacent samples across different encoders.
    const MARGIN: usize = RATE / 1000;
    let end = baseline.len().min(candidate.len()).saturating_sub(2 * MARGIN);
    if end < FRAME * RUN { return 0.; }
    let baseline = &baseline[MARGIN..MARGIN + end];
    let agreement = |offset: usize| {
        let mut dot = 0.;
        let mut energy = 0.;
        for (&a, &b) in baseline.iter().zip(&candidate[offset..]).step_by(32) {
            dot += f64::from(a) * f64::from(b);
            energy += f64::from(b).powi(2);
        }
        dot / energy.max(1.).sqrt()
    };
    let (_, shift) = (0..=2 * MARGIN).map(|offset| (agreement(offset), offset))
        .max_by(|a, b| a.0.total_cmp(&b.0)).unwrap();
    let candidate = &candidate[shift..shift + end];
    let mut differences: Vec<_> = baseline.as_chunks::<FRAME>().0.iter().zip(candidate.as_chunks::<FRAME>().0.iter())
        .map(|(a, b)| {
            let (mut aa, mut ab, mut bb) = (0_f64, 0_f64, 0_f64);
            for (&x, &y) in a.iter().zip(b).step_by(2) {
                let (x, y) = (f64::from(x), f64::from(y));
                aa += x * x; ab += x * y; bb += y * y;
            }
            if aa.min(bb) < 1e5 { return 1.; }
            (1. - ab / (aa * bb).sqrt()).max(0.) as f32
        }).collect();
    let mut sum = differences[..RUN].iter().sum::<f32>();
    let mut least = sum;
    for (old, new) in differences.iter().zip(&differences[RUN..]) {
        sum += new - old;
        least = least.min(sum);
    }
    let middle = differences.len() / 2;
    let typical = *differences.select_nth_unstable_by(middle, f32::total_cmp).1;
    typical / (least.max(0.) / RUN as f32 + 1e-4)
}

fn omission_to_review<'a>(
    original: &Recording, candidate: &Recording, lag: isize, checks: &'a [OmissionCheck], lyrics: &Lyrics,
    restored_ms: u32, current: &impl Fn() -> bool,
) -> Option<(&'a [OmissionCheck], Option<Interruption>)> {
    let mut risk = None;
    let mut hard: Option<(Interruption, &[OmissionCheck])> = None;
    for group in checks.chunk_by(|a, b| (a.from_ms, a.to_ms) == (b.from_ms, b.to_ms)) {
        if !current() { return None; }
        let key = (group[0].from_ms, group[0].to_ms);
        let baseline = original.cue(key, 0)?;
        let alternative = candidate.cue(key, lag)?;
        for mark in shared_interruptions(baseline, alternative, key, group, lyrics) {
            if let Some((existing, earlier)) = hard {
                // Two independent unexplained interruptions cannot be cleared by
                // reviewing one word. Leave this edition unconfirmed.
                // Overlapping cues may describe the same physical pause from
                // different lyric positions. One decode may clear it only when
                // both cues ask about the same sequence of words and anchors.
                if !mark.matches(&existing) || !same_omission_targets(earlier, group) { return None; }
            } else {
                hard = Some((mark, group));
            }
        }
        let distinct = !(key.0..=key.1).contains(&restored_ms);
        let score = retained_passage_score(baseline, alternative);
        if risk.as_ref().is_none_or(|&(other_distinct, other_score, _)| {
            (distinct, score) > (other_distinct, other_score)
        }) {
            risk = Some((distinct, score, group));
        }
    }
    if let Some((mark, group)) = hard { return Some((group, Some(mark))); }
    risk.map(|(_, _, group)| (group, None))
}

fn same_omission_targets(left: &[OmissionCheck], right: &[OmissionCheck]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)|
        a.missing == b.missing && a.before == b.before && a.after == b.after)
}

fn review_omissions(
    original: &Recording, candidate: &Recording, lag: isize, checks: &[OmissionCheck], lyrics: &Lyrics,
    restored_ms: u32, current: &impl Fn() -> bool,
) -> Option<()> {
    let (group, hard) = omission_to_review(original, candidate, lag, checks, lyrics, restored_ms, current)?;
    let key = (group[0].from_ms, group[0].to_ms);
    let model = full_speech_model()?.lock().ok()?;
    let mut state = model.create_state().ok()?;
    if !current() { return None; }
    if let Some(mark) = hard {
        review_shared_interruption(candidate, key, lag, group, lyrics, &mut state, mark, current)?;
    }
    if !current() { return None; }
    let words = omission_transcript(candidate.cue(key, lag)?, &mut state, 512)?;
    if group.iter().any(|check| supports_omission(&words, check)) {
        if !current() { return None; }
        let baseline = original.baseline_words(key, &mut state)?;
        if group.iter().any(|check| supports_omission(&baseline, check) && supports_omission(&words, check)) {
            return None;
        }
    }
    Some(())
}

// New words must belong to the expected line. Two consecutive words reduce ASR false positives.
fn restored(before: &[String], after: &[String], expected: &[String]) -> bool {
    after.windows(2).any(|pair| {
        expected.windows(2).any(|w| w == pair)
            && pair.iter().all(|w| !before.contains(w))
            && pair.iter().any(|w| w.chars().count() >= 4)
    }) || after.iter().any(|w| {
        expected.contains(w)
            && !before.contains(w)
            && ["нахуй", "пизд", "бляд", "ебан", "ебат", "хуй", "блять"]
                .iter()
                .any(|stem| w.starts_with(stem))
    })
}

fn has_incomplete_expected_gap(
    before: &[String], after: &[String], local_words: &[String], lyric_lines: &[Vec<String>],
) -> bool {
    after.iter().enumerate().any(|(spoken_index, word)| {
        if before.contains(word) || !local_words.contains(word) { return false; }
        lyric_lines.iter().any(|line| {
            line.iter().enumerate().any(|(target_index, target)| {
                if target != word { return false; }
                let Some(anchor_word) = after[..spoken_index].iter().rev()
                    .find(|spoken| before.contains(*spoken)) else { return false; };
                let Some(anchor_index) = (0..target_index).rev().find(|&i| line[i] == *anchor_word) else { return false; };
                line[anchor_index + 1..=target_index].iter().any(|expected| !after.contains(expected))
            })
        })
    })
}

fn audible_replacement(alternative: &[i16], from: u32, to: u32) -> bool {
    let Some(replacement) = alternative.get(from as usize * 16..to as usize * 16) else { return false; };
    let frames = replacement.as_chunks::<160>().0.iter();
    let total = frames.len();
    let audible = frames.filter(|frame| {
        let power = frame.iter().map(|&sample| i64::from(sample).pow(2)).sum::<i64>() / 160;
        power >= 107_374 // -40 dBFS
    }).count();
    total > 0 && audible * 2 >= total
}

fn timed_expected_word_in_cut(
    before: &[String], spoken: &[String], expected: &[String],
    timed: &[TimedWord], from_ms: u32, to_ms: u32,
) -> bool {
    expected.iter().filter(|word| word.chars().count() >= 4 && spoken.contains(word) && !before.contains(word))
        .any(|word| timed.iter().any(|span| span.word == *word
            && span.from_ms.saturating_add(180) >= from_ms
            && span.to_ms <= to_ms.saturating_add(180)))
}

fn nearby_audible_pause_interval(
    baseline: &[i16], alternative: &[i16], center_ms: u32,
) -> Option<(u32, u32)> {
    hard_pauses(baseline).into_iter().find(|&(from, to)| {
        center_ms.abs_diff(center_ms.clamp(from, to)) <= 280
            && audible_replacement(alternative, from, to)
    })
}

fn nearby_audible_pause(baseline: &[i16], alternative: &[i16], center_ms: u32) -> bool {
    nearby_audible_pause_interval(baseline, alternative, center_ms).is_some()
}

// A substantial hard pause shared by the candidate inside the baseline cut
// means that only part of a multiword edit may have been restored.
fn has_significant_residual_pause(baseline: &[i16], alternative: &[i16], center_ms: u32) -> bool {
    hard_pauses(baseline).into_iter().any(|(base_from, base_to)| {
        if center_ms.abs_diff(center_ms.clamp(base_from, base_to)) > 280 { return false; }
        let base_len = base_to.saturating_sub(base_from);
        hard_pauses(alternative).into_iter().any(|(alt_from, alt_to)| {
            let overlap = base_to.min(alt_to).saturating_sub(base_from.max(alt_from));
            overlap >= 180 && overlap.saturating_mul(2) >= base_len
        })
    })
}

fn localized_word_candidate(spoken: &[String], expected: &[String]) -> bool {
    expected.iter().any(|word| word.chars().count() >= 4 && spoken.contains(word))
}

// A single short word may be the entire edit. Its neighbors must survive in the
// original, where the word itself falls inside an audible cut in the mix.
fn word_restored_across_cut(
    previous: &[String], spoken: &[String], expected: &[String], context: &[String],
    baseline: &[i16], alternative: &[i16], center_ms: u32,
) -> bool {
    let anchored = spoken.windows(3).any(|phrase| {
        let word = &phrase[1];
        word.chars().count() >= 4 && expected.contains(word) && !previous.contains(word)
            && context.windows(3).any(|line| line == phrase)
            && (previous.windows(2).any(|pair| pair[0] == phrase[0] && pair[1] == phrase[2])
                || previous.windows(3).any(|line| line[0] == phrase[0] && line[2] == phrase[2]
                    && !expected.contains(&line[1])))
    });
    anchored && nearby_audible_pause(baseline, alternative, center_ms)
}

// A local decode around a hard cut can recover one expected word even when
// ASR cannot reliably transcribe its neighboring words in the full window.
fn localized_word_restored(
    previous: &[String], spoken: &[String], expected: &[String],
    base_crop: &[String], candidate_crop: &[String],
) -> bool {
    expected.iter().any(|word| {
        word.chars().count() >= 4
            && !previous.contains(word)
            && spoken.contains(word)
            && !base_crop.contains(word)
            && candidate_crop.contains(word)
    })
}

// Keep aligned audio, lyric context and cancellation explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn localized_word_restored_at_cut(
    previous: &[String], spoken: &[String], expected: &[String],
    baseline: &[i16], alternative: &[i16], center_ms: u32,
    state: &mut whisper_rs::WhisperState,
    current: &impl Fn() -> bool,
) -> bool {
    hard_pauses(baseline).into_iter().any(|(from, to)| {
        if center_ms.abs_diff(center_ms.clamp(from, to)) > 280
            || !audible_replacement(alternative, from, to) { return false; }
        let left = from.saturating_sub(200) as usize * 16;
        let right = (to + 200) as usize * 16;
        let (Some(base_crop), Some(candidate_crop)) = (baseline.get(left..right), alternative.get(left..right)) else { return false; };
        let Some(base_words) = transcript(base_crop, state) else { return false; };
        if !current() { return false; }
        let Some(candidate_words) = transcript(candidate_crop, state) else { return false; };
        if !localized_word_restored(previous, spoken, expected, &base_words, &candidate_words) { return false; }
        if !current() { return false; }
        let Some(timed) = transcript_timed_words(candidate_crop, state) else { return false; };
        timed_expected_word_in_cut(previous, spoken, expected, &timed,
            20, 380 + to.saturating_sub(from))
    })
}

// Rank sustained energy added in the speech band after compensating each bin for mastering gain.
fn added_speech_frames(before: &[i16], after: &[i16]) -> Vec<f32> {
    use symphonia_core::dsp::{complex::Complex, fft::Fft};
    const SIZE: usize = 512;
    const HOP: usize = 320;
    const BINS: usize = 110;
    let fft = Fft::new(SIZE);
    let window: Vec<_> = (0..SIZE)
        .map(|i| {
            (1.0 - (std::f32::consts::TAU * i as f32 / SIZE as f32).cos()) / (32768.0 * SIZE as f32)
        })
        .collect();
    let spectrum = |pcm: &[i16]| {
        let mut buffer = [Complex::default(); SIZE];
        for ((bin, sample), weight) in buffer.iter_mut().zip(pcm).zip(&window) {
            bin.re = *sample as f32 * weight;
        }
        fft.fft_inplace(&mut buffer);
        std::array::from_fn::<_, BINS, _>(|i| {
            let z = buffer[12 + i];
            (z.re * z.re + z.im * z.im).sqrt()
        })
    };
    let frames: Vec<_> = before
        .windows(SIZE)
        .step_by(HOP)
        .zip(after.windows(SIZE).step_by(HOP))
        .map(|(a, b)| {
            let x = spectrum(a);
            let y = spectrum(b);
            (
                std::array::from_fn::<_, BINS, _>(|i| ((y[i] + 0.0005) / (x[i] + 0.0005)).ln()),
                y,
            )
        })
        .collect();
    if frames.len() < 30 {
        return vec![];
    }
    let gain: [f32; BINS] = std::array::from_fn(|i| {
        let mut sampled: Vec<_> = frames
            .iter()
            .step_by(9)
            .map(|(ratio, _)| ratio[i])
            .collect();
        let middle = sampled.len() / 2;
        *sampled.select_nth_unstable_by(middle, f32::total_cmp).1
    });
    let added: Vec<f32> = frames
        .iter()
        .map(|(ratio, level)| {
            (0..BINS)
                .map(|i| {
                    let delta = ratio[i] - gain[i];
                    if level[i] > 0.002 && delta > 1.0 {
                        delta
                    } else {
                        0.
                    }
                })
                .sum::<f32>()
                / BINS as f32
        })
        .collect();
    added
}

fn added_speech_windows(before: &[i16], after: &[i16]) -> Vec<(f32, usize)> {
    let added = added_speech_frames(before, after);
    let mut scores: Vec<_> = added
        .windows(28)
        .step_by(5)
        .enumerate()
        .map(|(i, window)| (window.iter().sum::<f32>() / 28., i * 100 + 280))
        .collect();
    scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    scores
}

// A vocal edit may leave the instrumental backing audible. Localize only a
// short, sustained addition after compensating mastering gain; ASR still has to
// prove the expected word at this exact interval in both full and cropped audio.
fn spectral_intervals(activity: &[f32]) -> Vec<(u32, u32)> {
    spectral_intervals_up_to(activity, 85)
}

fn spectral_intervals_up_to(activity: &[f32], max_frames: usize) -> Vec<(u32, u32)> {
    let mut intervals = Vec::new();
    let mut i = 0;
    while i < activity.len() {
        if activity[i] <= 0.25 { i += 1; continue; }
        let start = i;
        let mut last = i;
        i += 1;
        while i < activity.len() && i - last <= 2 {
            if activity[i] > 0.25 { last = i; }
            i += 1;
        }
        let end = last + 1;
        if (5..=max_frames).contains(&(end - start)) {
            intervals.push((start as u32 * 20, end as u32 * 20 + 12));
        }
    }
    intervals
}

fn anchored_added_word<'a>(previous: &[String], spoken: &'a [String], expected: &[String], context: &[String]) -> Option<&'a str> {
    let mut words = spoken.windows(3).filter(|phrase| {
        phrase[1].chars().count() >= 4 && expected.contains(&phrase[1]) && !previous.contains(&phrase[1])
            && context.windows(3).any(|line| line == *phrase)
            && previous.windows(2).any(|pair| pair[0] == phrase[0] && pair[1] == phrase[2])
    }).map(|phrase| phrase[1].as_str());
    let word = words.next()?;
    words.all(|other| other == word).then_some(word)
}

// Fricatives and vowels of one word can form separate spectral islands. Join
// only nearby islands within the duration of a word, never a long mix change.
fn spectral_word_spans(activity: &[f32]) -> Vec<(u32, u32)> {
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for (from, to) in spectral_intervals(activity) {
        if let Some(last) = spans.last_mut() {
            if from.saturating_sub(last.1) <= 100 {
                last.1 = to;
                continue;
            }
        }
        spans.push((from, to));
    }
    spans.retain(|&(from, to)| (180..=1700).contains(&(to - from)));
    spans
}

fn timed_spectral_word(word: &str, timed: &[TimedWord], from: u32, to: u32) -> bool {
    timed.iter().any(|span| {
        if span.word != word || span.to_ms <= span.from_ms { return false; }
        let length = span.to_ms - span.from_ms;
        let overlap = span.to_ms.min(to).saturating_sub(span.from_ms.max(from));
        overlap >= 80 && overlap * 4 >= length * 3
            && span.from_ms.saturating_add(180) >= from
            && span.to_ms <= to.saturating_add(180)
    })
}

// Different recognizers can hear different words in the same performance.
// A remote transcript is therefore only a proposal: the proposed new word
// must overlap a short physical addition at the edit, not elsewhere in the clip.
fn timed_new_word_at_edit(
    previous: &[String], spoken: &[String], expected: &[String],
    timed: &[TimedWord], intervals: &[(u32,u32)], center_ms: u32,
) -> bool {
    expected.iter().filter(|word| word.chars().count() >= 4
        && !previous.contains(word) && spoken.contains(word)).any(|word| {
        intervals.iter().any(|&(from,to)| {
            center_ms.abs_diff(center_ms.clamp(from,to)) <= 280
                && timed_spectral_word(word,timed,from,to)
        })
    })
}

// A multiword mute can be longer than a single-word spectral island. Require
// two adjacent expected words, each physically suppressed by at least 20 dB
// relative to the surrounding aligned recording. A mastering gain or an ASR
// disagreement over an otherwise audible phrase is not enough.
fn suppressed_phrase_at_edit(
    previous: &[String], expected: &[String], timed: &[TimedWord],
    baseline: &[i16], alternative: &[i16], activity: &[f32], center_ms: u32,
) -> bool {
    let spans = spectral_intervals_up_to(activity, 150).into_iter()
        .filter(|&(from,to)| to - from > 1700
            && center_ms.abs_diff(center_ms.clamp(from,to)) <= 280).collect::<Vec<_>>();
    if spans.is_empty() || baseline.len() != alternative.len() { return false; }
    let power = |pcm: &[i16]| pcm.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
    let before_total = power(baseline);
    let after_total = power(alternative);
    let suppressed = |word: &TimedWord| {
        let duration = word.to_ms.saturating_sub(word.from_ms);
        if !(160..=1700).contains(&duration) { return false; }
        let from = word.from_ms as usize * 16;
        let to = word.to_ms as usize * 16;
        let (Some(before),Some(after)) = (baseline.get(from..to),alternative.get(from..to)) else { return false; };
        let before_word=power(before); let after_word=power(after);
        let before_rest=before_total-before_word; let after_rest=after_total-after_word;
        let rest_len=baseline.len()-before.len();
        rest_len >= 8000 && before_rest/rest_len as f64 >= 427_464.
            && after_rest > 0. && after_word/after.len() as f64 >= 427_464.
            && before_word * after_rest <= 0.01 * after_word * before_rest
    };
    timed.windows(2).any(|pair| {
        pair[1].from_ms >= pair[0].to_ms && pair[1].from_ms - pair[0].to_ms <= 500
            && expected.windows(2).any(|words| words[0] == pair[0].word && words[1] == pair[1].word)
            && pair.iter().all(|word| word.word.chars().count() >= 4
                && !previous.contains(&word.word) && suppressed(word))
            && spans.iter().any(|&(from,to)| pair.iter().all(|word|
                timed_spectral_word(&word.word,std::slice::from_ref(word),from,to)))
    })
}

fn remote_word_at_audio_edit(
    previous: &[String], spoken: &[String], expected: &[String], timed: &[TimedWord],
    baseline: &[i16], alternative: &[i16], center_ms: u32,
) -> bool {
    let activity = added_speech_frames(baseline,alternative);
    let mut intervals = spectral_word_spans(&activity);
    intervals.extend(hard_pauses(baseline).into_iter()
        .filter(|&(from,to)| audible_replacement(alternative,from,to))
        .map(|(from,to)| (from.saturating_sub(180),to.saturating_add(180))));
    timed_new_word_at_edit(previous,spoken,expected,timed,&intervals,center_ms)
        || suppressed_phrase_at_edit(previous,expected,timed,baseline,alternative,&activity,center_ms)
}

// Keep aligned audio, lyric context and cancellation explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn localized_spectral_word(
    previous: &[String], spoken: &[String], word: &str,
    baseline: &[i16], alternative: &[i16], center_ms: u32,
    state: &mut whisper_rs::WhisperState, current: &impl Fn() -> bool,
) -> bool {
    if previous.iter().any(|v| v == word) || !spoken.iter().any(|v| v == word) { return false; }
    let spans: Vec<_> = spectral_word_spans(&added_speech_frames(baseline, alternative))
        .into_iter().filter(|&(from, to)| center_ms.abs_diff(center_ms.clamp(from, to)) <= 280).collect();
    if spans.is_empty() || !current() { return false; }
    // The paired context already proves the target and its surviving neighbours.
    // Locate that same word inside the added voice, keeping the full context for
    // recognition instead of re-decoding a subsecond isolated syllable three times.
    let Some(timed) = transcript_timed_words(alternative, state) else { return false; };
    current() && spans.into_iter().any(|(from, to)| timed_spectral_word(word, &timed, from, to))
}

#[cfg(test)]
fn compare(original: &Recording, candidate: &Recording, lyrics: &Lyrics) -> Option<f32> {
    compare_while_current(original, candidate, lyrics, &|| true)
}

pub(crate) fn compare_while_current(
    original: &Recording, candidate: &Recording, lyrics: &Lyrics, current: &impl Fn() -> bool,
) -> Option<f32> {
    if matches!(lyrics.omission_plan, OmissionPlan::Incomplete) || !current() { return None; }
    if original.digest == candidate.digest {
        return None;
    }
    let lag = alignment(&original.envelope, &candidate.envelope)?;
    if !silent_extra_tail(&original.pcm, &candidate.pcm, lag) { return None; }
    let time = restored_time(original, candidate, lyrics, lag, 12, current)?;
    // A different mix can add words at one point while removing them elsewhere.
    // Review only the two strongest reverse differences, not every lyric fragment.
    if restored_time(candidate, original, lyrics, -lag, 2, current).is_some() { return None; }
    let restored_ms = (time * 1000.).round() as u32;
    if has_remaining_mono_splice(original, candidate, lag, restored_ms) { return None; }
    if let OmissionPlan::Complete(checks) = &lyrics.omission_plan {
        if !checks.is_empty() {
            review_omissions(original, candidate, lag, checks, lyrics, restored_ms, current)?;
        }
    }
    current().then_some(time)
}

// Neighbours may cross a synced-line boundary. Keep them within the actual
// ASR crop; the restored target itself must still belong to the edit's line.
fn lyric_context(lyrics: &Lyrics, begin_ms: usize, end_ms: usize, unsynced: &[String]) -> Vec<String> {
    if lyrics.synced.is_empty() { return unsynced.to_vec(); }
    let begin = begin_ms as f32 / 1000.;
    let end = end_ms as f32 / 1000.;
    let lines = lyrics.synced.iter()
        .filter(|line| line.from < end && line.to.unwrap_or(line.from + 6.) > begin)
        .map(|line| line.text.as_str()).collect::<Vec<_>>().join(" ");
    words(&lines)
}

fn restored_time(original: &Recording, candidate: &Recording, lyrics: &Lyrics, lag: isize, max_windows: usize, current: &impl Fn() -> bool) -> Option<f32> {
    let (a, b) = if lag >= 0 {
        (0, lag as usize)
    } else {
        ((-lag) as usize, 0)
    };
    let len = (original.envelope.len() - a).min(candidate.envelope.len() - b);
    let scores = added_speech_windows(
        &original.pcm[a * 16..(a + len) * 16],
        &candidate.pcm[b * 16..(b + len) * 16],
    );
    struct SpeechWindow<'a> {
        sec: f32,
        baseline: &'a [i16],
        candidate: &'a [i16],
        expected: Vec<String>,
        context: Vec<String>,
        center_ms: u32,
    }
    let mut windows = Vec::new();
    let mut context_windows = Vec::new();
    let mut tried = Vec::new();
    let all_text = lyrics.text.join(" ");
    let lyric_lines: Vec<_> = lyrics.text.iter().map(|line| words(line)).collect();
    for (score, offset) in scores {
        if score < 0.14 || tried.len() >= max_windows {
            break;
        }
        let pos = a + offset;
        if tried
            .iter()
            .any(|&other: &usize| other.abs_diff(pos) < 1800)
        {
            continue;
        }
        let sec = pos as f32 / 1000.;
        let line = lyrics
            .synced
            .iter()
            .filter(|l| l.from <= sec + 0.35 && l.to.unwrap_or(l.from + 6.) >= sec - 0.35)
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let expected = words(if lyrics.synced.is_empty() {
            &all_text
        } else {
            &line
        });
        if expected.is_empty() {
            continue;
        }
        tried.push(pos);
        let begin = pos.saturating_sub(2500).max(a);
        let end = (pos + 2900).min(a + len);
        let at = begin.checked_add_signed(lag)?;
        if windows.len() < 2 && begin >= a + 500 {
            // A word at the start of a short ASR crop can be misheard. Give only
            // the strongest two edits another 500 ms of leading context, keeping
            // the same duration, alignment, expected lyric and acceptance checks.
            context_windows.push(SpeechWindow {
                sec,
                baseline: &original.pcm[(begin - 500) * 16..(end - 500) * 16],
                candidate: &candidate.pcm[(at - 500) * 16..(at + end - begin - 500) * 16],
                expected: expected.clone(),
                context: lyric_context(lyrics, begin - 500, end - 500, &expected),
                center_ms: (pos - begin + 500) as u32,
            });
        }
        windows.push(SpeechWindow {
            sec,
            baseline: &original.pcm[begin * 16..end * 16],
            candidate: &candidate.pcm[at * 16..(at + end - begin) * 16],
            context: lyric_context(lyrics, begin, end, &expected),
            expected,
            center_ms: (pos - begin) as u32,
        });
    }
    if windows.is_empty() {
        return None;
    }
    if !current() { return None; }
    // Start with the strongest edit. Sending several later cues up front delays
    // an already provable first word and can exhaust the inference deadline.
    // Only external-candidate slices map to server times; originals stay local.
    remote_speech::prefetch(windows.iter().take(1).flat_map(|window| [window.candidate,window.baseline]));
    let context_candidates = RefCell::new(std::collections::HashSet::new());
    let check = |model: &whisper_rs::WhisperContext, windows: &[SpeechWindow<'_>], full_model: bool| {
        if windows.is_empty() { return None; }
        // full() replaces the output segments; no_context keeps each clip independent.
        let mut state = model.create_state().ok()?;
        windows.iter().take_while(|_| current()).find_map(|window| {
            let added = transcript(window.candidate, &mut state)?;
            let prefilter = restored(&[], &added, &window.expected);
            let local_candidate = !prefilter
                && localized_word_candidate(&added, &window.expected)
                && nearby_audible_pause(window.baseline, window.candidate, window.center_ms);
            if !prefilter && !local_candidate { return None; }
            if !current() { return None; }
            let previous = original.cached_words(window.baseline, full_model, || transcript(window.baseline, &mut state))?;
            if window.expected.iter().any(|word| word.chars().count() >= 4
                && added.contains(word) && !previous.contains(word)) {
                context_candidates.borrow_mut().insert(window.sec.to_bits());
            }
            if remote_speech::has_transcript(window.candidate) {
                let timed = transcript_timed_words(window.candidate, &mut state)?;
                if !remote_word_at_audio_edit(&previous, &added, &window.expected, &timed,
                    window.baseline, window.candidate, window.center_ms) { return None; }
            }
            if let Some(word) = anchored_added_word(&previous, &added, &window.expected, &window.context) {
                if !nearby_audible_pause(window.baseline, window.candidate, window.center_ms)
                    && !has_significant_residual_pause(window.baseline, window.candidate, window.center_ms)
                    && localized_spectral_word(&previous, &added, word, window.baseline, window.candidate,
                        window.center_ms, &mut state, current) {
                    return Some(window.sec);
                }
            }
            let normal = restored(&previous, &added, &window.expected);
            if normal {
                let legacy = matches!(&lyrics.omission_plan, OmissionPlan::Legacy);
                let possible_incomplete_gap = legacy
                    && has_incomplete_expected_gap(&previous, &added, &added, &lyric_lines);
                if has_significant_residual_pause(window.baseline, window.candidate, window.center_ms) {
                    return None;
                }
                let pause = nearby_audible_pause_interval(
                    window.baseline, window.candidate, window.center_ms,
                );
                let mut timed = None;
                if possible_incomplete_gap {
                    let words = transcript_timed_words(window.candidate, &mut state)?;
                    let local_words: Vec<_> = words.iter()
                        .filter(|word| word.to_ms.saturating_add(700) >= window.center_ms
                            && word.from_ms <= window.center_ms.saturating_add(700))
                        .map(|word| word.word.clone()).collect();
                    if has_incomplete_expected_gap(&previous, &added, &local_words, &lyric_lines) {
                        return None;
                    }
                    timed = Some(words);
                }
                if let Some((from, to)) = pause {
                    if timed.is_none() {
                        timed = Some(transcript_timed_words(window.candidate, &mut state)?);
                    }
                    return timed_expected_word_in_cut(&previous, &added, &window.expected,
                        timed.as_deref()?, from.saturating_sub(180), to.saturating_add(180))
                        .then_some(window.sec);
                }
                return Some(window.sec);
            }
            let short = word_restored_across_cut(&previous, &added, &window.expected, &window.context,
                window.baseline, window.candidate, window.center_ms);
            if short {
                if has_significant_residual_pause(window.baseline, window.candidate, window.center_ms) {
                    return None;
                }
                return Some(window.sec);
            }
            if !local_candidate || !current() { return None; }
            if has_significant_residual_pause(window.baseline, window.candidate, window.center_ms) {
                return None;
            }
            localized_word_restored_at_cut(&previous, &added, &window.expected,
                window.baseline, window.candidate, window.center_ms, &mut state, current)
                .then_some(window.sec)
        })
    };
    let fast = if lyrics.synced.is_empty() { 0 } else { windows.len().min(3) };
    if fast > 0 {
        if let Some(model) = crate::language::whisper_model() {
            if let Some(time) = check(&model, &windows[..fast], false) { return Some(time); }
        }
    }
    if let Some(model) = full_speech_model() {
        let model = model.lock().ok()?;
        let strongest = windows.len().min(2);
        if let Some(time) = check(&model, &windows[..strongest], true) { return Some(time); }
        // Retry only when the first crop contained a new expected word; unrelated
        // recordings and unrecognizable speech must not spend another ASR pass.
        context_windows.retain(|window| context_candidates.borrow().contains(&window.sec.to_bits()));
        return check(&model, &context_windows, true)
            .or_else(|| check(&model, &windows[strongest..], true));
    }
    let model = crate::language::whisper_model()?;
    check(&model, &windows[fast..], false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn baseline_transcripts_are_bound_to_exact_slice_model_and_recording() {
        let recording = Recording { pcm: vec![0; RATE*4], envelope:vec![],mono_splices:vec![],
            reference_transcripts:RefCell::default(),speech_transcripts:RefCell::default(),digest:[0;32] };
        let first = &recording.pcm[..RATE];
        assert_eq!(recording.cached_words(first,false,|| Some(words("tiny"))),Some(words("tiny")));
        assert_eq!(recording.cached_words(first,false,|| panic!("same slice and model must reuse recognition")),Some(words("tiny")));
        assert_eq!(recording.cached_words(first,true,|| Some(words("full"))),Some(words("full")));
        assert_eq!(recording.cached_words(&recording.pcm[16..RATE+16],false,|| Some(words("shifted"))),Some(words("shifted")));
        let outside = vec![0; RATE];
        assert_eq!(recording.cached_words(&outside,false,|| Some(words("outside"))),Some(words("outside")));
        assert_eq!(recording.cached_words(&outside,false,|| None),None,"foreign slices must not be cached");
        let failed = &recording.pcm[RATE..RATE*2];
        assert_eq!(recording.cached_words(failed,false,|| None),None);
        assert_eq!(recording.cached_words(failed,false,|| Some(words("recovered"))),Some(words("recovered")));
        remote_speech::with_candidate(&recording.pcm,remote_speech::Provider::new(|_,_| None),|| {
            assert_eq!(recording.cached_words(first,false,|| Some(words("candidate"))),Some(words("candidate")),
                "candidate assistance must not reuse or overwrite local baseline evidence");
        });
        assert_eq!(recording.cached_words(first,false,|| panic!("local evidence must survive candidate scope")),Some(words("tiny")));
    }

    #[test]
    fn long_phrase_requires_two_suppressed_words_and_rejects_mastering_gain() {
        let alternative=vec![2000i16;16000*6]; let mut baseline=alternative.clone();
        baseline[16000..16000*3+3200].fill(10);
        let mut activity=vec![0.;300];activity[50..160].fill(1.);
        let timed=vec![TimedWord{word:"minimum".into(),from_ms:1400,to_ms:1800},TimedWord{word:"seven".into(),from_ms:2100,to_ms:2400}];
        let expected=words("minimum seven");
        let accept=|before:&[i16],timed:&[TimedWord]| suppressed_phrase_at_edit(&[],&expected,timed,before,&alternative,&activity,2000);
        assert!(accept(&baseline,&timed));
        assert!(!accept(&baseline,&timed[..1]));
        assert!(!accept(&vec![10i16;alternative.len()],&timed),"global gain is not a restored phrase");
        assert!(!accept(&alternative,&timed),"different ASR words cannot prove an audible change");
        let outside=vec![TimedWord{word:"minimum".into(),from_ms:4000,to_ms:4400},TimedWord{word:"seven".into(),from_ms:4600,to_ms:4900}];
        assert!(!accept(&baseline,&outside));
    }

    #[test]
    fn lyric_numbers_match_spoken_numbers_without_inventing_decimal_words() {
        let expected = words("ещё минимум 7 грамм");
        let spoken = words("ещё минимум семь грамм");
        assert_eq!(expected, spoken);
        assert!(restored(&words("ещё"), &spoken, &expected));
        assert!(!restored(&expected, &spoken, &expected));
        assert_eq!(words("(21), 30!"), words("двадцать один тридцать"));
        for notation in ["7.5", "7,5", "7/5", "007", "a7", "7-летний", "+7", "-7"] {
            assert!(!words(notation).iter().any(|word| word == "семь"), "{notation}");
        }
    }

    #[test]
    fn different_asr_words_cannot_prove_an_unrelated_audio_edit() {
        let previous = words("остался рядом");
        let spoken = words("остался рядом снова сегодня");
        let expected = spoken.clone();
        assert!(restored(&previous,&spoken,&expected));
        let timed = [TimedWord {word:"снова".into(),from_ms:3000,to_ms:3500},
            TimedWord {word:"сегодня".into(),from_ms:3600,to_ms:4100}];
        assert!(!timed_new_word_at_edit(&previous,&spoken,&expected,&timed,&[(900,1500)],1200));
        assert!(!timed_new_word_at_edit(&previous,&spoken,&expected,&timed,&[(2950,3550)],1200));
        assert!(timed_new_word_at_edit(&previous,&spoken,&expected,&timed,&[(2950,3550)],3200));
        assert!(!timed_new_word_at_edit(&spoken,&spoken,&expected,&timed,&[(2950,3550)],3200));
    }

    #[test]
    fn spectral_word_needs_its_full_timed_span_not_a_different_or_partial_edit() {
        let target = TimedWord { word: "слово".into(), from_ms: 1000, to_ms: 1600 };
        assert!(timed_spectral_word("слово", std::slice::from_ref(&target), 980, 1630));
        assert!(!timed_spectral_word("другое", std::slice::from_ref(&target), 980, 1630));
        assert!(!timed_spectral_word("слово", std::slice::from_ref(&target), 2000, 2600));
        assert!(!timed_spectral_word("слово", &[target], 1000, 1300));
        assert!(!timed_spectral_word("слово", &[TimedWord { word: "слово".into(), from_ms: 1200, to_ms: 1200 }], 1000, 1600));
        let mut activity = vec![0.; 140];
        activity[30..43].fill(0.6);
        activity[46..60].fill(0.6);
        assert_eq!(spectral_word_spans(&activity), [(600, 1212)]);
        activity[80..95].fill(0.6);
        assert_eq!(spectral_word_spans(&activity), [(600, 1212), (1600, 1912)]);
        activity.fill(0.6);
        assert!(spectral_word_spans(&activity).is_empty());
    }

    #[test]
    fn spectral_word_requires_expected_surviving_neighbors_and_local_change() {
        let expected = words("выбери слово сегодня снова");
        assert_eq!(anchored_added_word(&words("выбери сегодня"), &expected, &expected, &expected), Some("слово"));
        assert_eq!(anchored_added_word(&expected, &expected, &expected, &expected), None);
        assert_eq!(anchored_added_word(&words("выбери завтра"), &expected, &expected, &expected), None);
        assert_eq!(anchored_added_word(&words("выбери сегодня"), &words("выбери другое сегодня"), &expected, &expected), None);
        let mut activity = vec![0.; 120];
        activity[10..20].fill(0.6);
        assert_eq!(spectral_intervals(&activity), [(200, 412)]);
        activity.fill(0.6);
        assert!(spectral_intervals(&activity).is_empty());
        activity.fill(0.);
        activity[10..14].fill(0.6);
        assert!(spectral_intervals(&activity).is_empty());
    }

    #[test]
    fn restored_word_uses_next_line_only_for_its_surviving_neighbor() {
        let target_line = words("выбери слово");
        let context = words("выбери слово сегодня снова");
        let previous = words("выбери сегодня");
        let spoken = words("выбери слово сегодня");
        assert_eq!(anchored_added_word(&previous, &spoken, &target_line, &target_line), None);
        assert_eq!(anchored_added_word(&previous, &spoken, &target_line, &context), Some("слово"));
        assert_eq!(anchored_added_word(&words("слово снова"), &words("слово сегодня снова"),
            &target_line, &context), None); // A neighbour cannot become the target.
        assert_eq!(anchored_added_word(&words("выбери завтра"), &spoken, &target_line, &context), None);
        assert_eq!(anchored_added_word(&spoken, &spoken, &target_line, &context), None);
    }

    #[test]
    fn lyric_neighbors_follow_each_crop_including_its_shifted_boundary() {
        let line = |from, to, text: &str| crate::api::SyncLine { from, to: Some(to), text: text.into() };
        let lyrics = Lyrics { synced: vec![line(0., 2., "начальный фрагмент"),
            line(2., 4., "следующий сосед"), line(4., 6., "дальний фрагмент")], ..Default::default() };
        assert_eq!(lyric_context(&lyrics, 1000, 3000, &[]), words("начальный фрагмент следующий сосед"));
        assert_eq!(lyric_context(&lyrics, 0, 2000, &[]), words("начальный фрагмент"));
        assert_eq!(lyric_context(&lyrics, 500, 2500, &[]), words("начальный фрагмент следующий сосед"));
        assert_eq!(lyric_context(&lyrics, 2000, 4000, &[]), words("следующий сосед"));
        assert!(lyric_context(&lyrics, 6000, 7000, &[]).is_empty());
        assert_eq!(lyric_context(&Lyrics::default(), 0, 1000, &words("обычный текст")), words("обычный текст"));
    }

    #[test]
    fn duration_difference_requires_silent_aligned_tail() {
        let candidate = vec![2000; 8 * RATE];
        let mut original = candidate.clone();
        original.extend(vec![3; 4 * RATE]);
        assert!(silent_extra_tail(&original, &candidate, 0));
        let mut delayed = vec![0; RATE / 5];
        delayed.extend(&candidate);
        assert!(silent_extra_tail(&original, &delayed, 200));
        original[10 * RATE..10 * RATE + RATE / 100].fill(1000);
        assert!(!silent_extra_tail(&original, &candidate, 0));
        assert!(!silent_extra_tail(&original, &delayed, 200));
        assert!(!silent_extra_tail(&candidate, &original, 0));
        assert!(!silent_extra_tail(&original, &candidate, 20_000));
        let mut shorter_gap = candidate.clone();
        shorter_gap.extend(vec![1000; 2 * RATE]);
        assert!(!silent_extra_tail(&shorter_gap, &candidate, 0));
    }

    #[test]
    fn persistent_mono_is_only_suspicious_after_a_matching_restore() {
        let recording = |marks: &[(u32, u32)]| Recording {
            pcm: Vec::new(), envelope: Vec::new(), mono_splices: marks.to_vec(),
            reference_transcripts: RefCell::default(), speech_transcripts: RefCell::default(), digest: [0; 32],
        };
        let original = recording(&[(1100, 1530), (3750, 4510)]);
        let partial = recording(&[(3770, 4530)]);
        assert!(has_remaining_mono_splice(&original, &partial, 20, 1250));
        assert!(!has_remaining_mono_splice(&original, &recording(&[]), 20, 1250));
        assert!(!has_remaining_mono_splice(&original, &partial, 20, 2500));
        assert!(!has_remaining_mono_splice(&original, &original, 0, 1250));

        let mut stereo = vec![(false, true); 160];
        stereo[60..100].fill((true, false));
        assert_eq!(mono_splices(&stereo), [(600, 1000)]);
        stereo.fill((true, false));
        assert!(mono_splices(&stereo).is_empty());
    }

    #[test]
    fn one_review_cannot_clear_two_independent_hard_edits_or_different_targets() {
        let mut pcm = vec![2000; 6 * RATE];
        let gap = |ms: usize| ms * (RATE / 1000);
        pcm[gap(1200)..gap(1550)].fill(0);
        pcm[gap(3500)..gap(3800)].fill(0);
        let recording = Recording { pcm, envelope: Vec::new(), mono_splices: Vec::new(), reference_transcripts: RefCell::default(), speech_transcripts: RefCell::default(), digest: [0; 32] };
        let lyrics = Lyrics { synced: vec![crate::api::SyncLine {
            from: 0., to: Some(6.), text: "наступило утро солнце сияет".into(),
        }], ..Default::default() };
        let cue = |from_ms, to_ms, missing: &str| OmissionCheck {
            from_ms, to_ms, missing: missing.into(),
            before: ["вот".into(), "и".into()], after: ["еще".into(), "день".into()],
        };
        let first = cue(500, 2200, "утро");
        let review = |checks: &[OmissionCheck]| {
            omission_to_review(&recording, &recording, 0, checks, &lyrics, 5200, &|| true)
                .and_then(|(_, mark)| mark).map(|mark| mark.from)
        };
        assert_eq!(review(&[first.clone(), cue(600, 2000, "утро")]), Some(1200));
        assert_eq!(review(&[first.clone(), cue(600, 2000, "солнце")]), None);
        assert_eq!(review(&[first, cue(2800, 4300, "утро")]), None);
    }

    #[test]
    fn ranks_a_retained_passage_after_submillisecond_alignment() {
        let mut seed = 1_u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 17) as i16 - 16384
        };
        let base: Vec<_> = (0..3 * RATE).map(|_| noise()).collect();
        let alternative: Vec<_> = base.iter().map(|x| x / 2 + noise() / 2).collect();
        let mut partial = alternative.clone();
        for i in RATE..(2 * RATE) { partial[i] = base[i] / 2; }
        for offset in [0, 5, 15] {
            let align = |audio: &[i16]| {
                let mut shifted = vec![0; offset];
                shifted.extend_from_slice(audio);
                shifted
            };
            let risk = retained_passage_score(&base, &align(&partial));
            assert!(risk > retained_passage_score(&base, &align(&alternative)));
        }
    }

    #[test]
    fn one_retained_word_cannot_clear_two_positions_in_the_same_cue() {
        let target = |missing: &str, before: &str, after: &str| OmissionCheck {
            from_ms: 0, to_ms: 1000, missing: missing.into(),
            before: ["это".into(), before.into()], after: [after.into(), "снова".into()],
        };
        let first = target("утро", "наступило", "сегодня");
        let second = target("солнце", "сияет", "ярко");
        let words = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let heard = |s: &str| heard_cue_targets(&words(s), &[&first, &second]);
        assert!(heard("наступило утро и солнце ярко"));
        assert!(!heard("сияет солнце ярко"));
        assert!(!heard("сияет солнце а наступило утро"));
        assert!(!heard("утро было и солнце светит"));
        assert!(!heard_cue_targets(&words("наступило утро"), &[&first, &first]));
        assert!(heard_cue_targets(&words("утро"), &[&first]));
    }

    #[test]
    fn short_word_restore_requires_muted_baseline_anchors_and_nearby_audible_cut() {
        let phrase = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let expected = phrase("тихо ругань снова");
        let before = phrase("тихо снова");
        let after = phrase("тихо ругань снова");
        let mut baseline = vec![1200i16; 1600 * 16];
        baseline[520 * 16..720 * 16].fill(0);
        let alternative = vec![1200i16; 1600 * 16];
        assert!(word_restored_across_cut(&before, &after, &expected, &expected, &baseline, &alternative, 620));
        assert!(!word_restored_across_cut(&after, &before, &expected, &expected, &alternative, &baseline, 620));
        assert!(!word_restored_across_cut(&before, &phrase("рядом ругань снова"), &expected, &expected, &baseline, &alternative, 620));
        assert!(!word_restored_across_cut(&before, &after, &expected, &expected, &baseline, &alternative, 1100));
        let intact = vec![1200i16; 1600 * 16];
        assert!(!word_restored_across_cut(&before, &after, &expected, &expected, &intact, &alternative, 620));
        let silent = vec![0i16; 1600 * 16];
        assert!(!word_restored_across_cut(&before, &after, &expected, &expected, &baseline, &silent, 620));
    }

    #[test]
    fn restored_word_must_time_align_inside_the_edit_cut() {
        let expected = ["наши", "деньги", "наша", "участь"].map(str::to_owned);
        let before = ["наши", "наша", "участь"].map(str::to_owned);
        let spoken = ["наши", "деньги", "наша", "участь"].map(str::to_owned);
        let inside = [TimedWord { word: "деньги".into(), from_ms: 1010, to_ms: 1610 }];
        let edge = [TimedWord { word: "наша".into(), from_ms: 1720, to_ms: 2010 }];
        assert!(timed_expected_word_in_cut(&before, &spoken, &expected, &inside, 1000, 1700));
        assert!(!timed_expected_word_in_cut(&before, &spoken, &expected, &edge, 1000, 1700));
        assert!(!timed_expected_word_in_cut(&before, &before, &expected, &inside, 1000, 1700));
    }

    #[test]
    fn significant_shared_pause_rejects_partial_cut_but_not_filled_cut() {
        let mut baseline = vec![10_000_i16; 3_000 * 16];
        let mut partial = baseline.clone();
        let complete = baseline.clone();
        baseline[500 * 16..1_640 * 16].fill(0);
        partial[500 * 16..1_240 * 16].fill(0);

        assert!(has_significant_residual_pause(&baseline, &partial, 1_000));
        assert!(!has_significant_residual_pause(&baseline, &complete, 1_000));
        assert!(!has_significant_residual_pause(&baseline, &partial, 2_600));
    }

    #[test]
    fn normal_restore_must_include_the_whole_expected_gap_after_shared_anchor() {
        let before = words("alpha beta noise tail");
        let partial = words("alpha beta second tail");
        let complete = words("alpha beta first second tail");
        let lyrics = vec![words("alpha beta first second tail")];

        assert!(has_incomplete_expected_gap(&before, &partial, &partial, &lyrics));
        assert!(!has_incomplete_expected_gap(&before, &complete, &complete, &lyrics));
    }

    #[test]
    fn localized_short_word_requires_expected_candidate_and_baseline_absence() {
        let expected = ["синяя", "звезда", "снова", "встала"].map(str::to_owned);
        let spoken = ["помеха", "звезда", "вдруг"].map(str::to_owned);
        let candidate_crop = ["звезда", "снова", "встала"].map(str::to_owned);
        let previous = ["помеха", "тропа", "вдруг"].map(str::to_owned);
        let base_crop = ["звёзд", "вдруг"].map(str::to_owned);
        assert!(localized_word_restored(&previous, &spoken, &expected, &base_crop, &candidate_crop));
        assert!(!localized_word_restored(&spoken, &previous, &expected, &candidate_crop, &base_crop));
        assert!(!localized_word_restored(&previous, &spoken, &expected, &candidate_crop, &candidate_crop));
        assert!(!localized_word_restored(&previous, &spoken, &expected, &base_crop, &base_crop));
        assert!(!localized_word_restored(&previous, &spoken, &expected, &base_crop, &previous));
    }

    #[test]
    fn a_tone_in_music_requires_sustained_purity_and_matching_frequency() {
        let sample = |i: usize, hz: f64| {
            (6000. * (i as f64 * hz * std::f64::consts::TAU / RATE as f64).sin()) as i16
        };
        let music: Vec<_> = (0..RATE * 2).map(|i| sample(i, 382.) + sample(i, 659.) + sample(i, 827.)).collect();
        let tone = |hz| {
            let mut audio = music.clone();
            for (i, x) in audio[RATE / 2..RATE].iter_mut().enumerate() { *x = sample(i, hz); }
            interruptions(&audio)
        };
        assert!(interruptions(&music).is_empty());
        let mut noise = 1_u32;
        let random: Vec<_> = (0..RATE * 2).map(|_| {
            noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
            (noise >> 16) as i16
        }).collect();
        assert!(interruptions(&random).is_empty());
        let standard = tone(1000.);
        assert_eq!(standard.len(), 1);
        assert!(standard[0].from.abs_diff(500) <= 10 && standard[0].to.abs_diff(1000) <= 10);
        assert!(standard[0].matches(&tone(1003.)[0]));
        assert!(!standard[0].matches(&tone(1375.)[0]));
        assert!(!standard[0].matches(&Interruption { from: 500, to: 1000, tone_hz: None }));
    }

    #[test]
    fn hard_pause_requires_a_sustained_quiet_run_with_two_audible_flanks() {
        let mut pcm = vec![0i16; RATE * 2];
        pcm[..8000].fill(1200);
        pcm[16000..].fill(-1200);
        assert_eq!(hard_pauses(&pcm), [(500, 1000)]);
        pcm[8000..16000].fill(48);
        assert!(hard_pauses(&pcm).is_empty());
        pcm[..16000].fill(0);
        assert!(hard_pauses(&pcm).is_empty());
    }

    #[test]
    fn omitted_word_requires_adjacent_anchors_and_absent_target() {
        let check = OmissionCheck { from_ms: 0, to_ms: 1000, missing: "слово".into(),
            before: ["он".into(), "в".into()], after: ["и".into(), "ты".into()] };
        let evidence = |s: &str| supports_omission(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>(), &check);
        assert!(evidence("он в и ты"));
        assert!(!evidence("он в слово и ты"));
        assert!(!evidence("он в ещё и ты"));
        assert!(!evidence("слово он в и ты"));
        assert!(!evidence("в и ты"));
    }

    #[test]
    #[ignore = "requires local catalog audio, external audio, and a matching LRCLIB response"]
    fn catalog_recording_pair() {
        let env = |key| std::env::var(key).unwrap();
        let ms = env("YM_DURATION_MS").parse().unwrap();
        let read = |key| {
            let file = std::fs::File::open(env(key)).unwrap();
            Recording::decode(rodio::Decoder::new(std::io::BufReader::new(file)).unwrap(), ms).unwrap()
        };
        let baseline = read("YM_BASELINE_AUDIO");
        let candidate = read("YM_EXTERNAL_AUDIO");
        let data: serde_json::Value = serde_json::from_slice(&std::fs::read(env("YM_REFERENCE_LYRICS")).unwrap()).unwrap();
        let lyrics = crate::api::reference_row(&data, ms);
        let result = compare(&baseline, &candidate, &lyrics);
        eprintln!("restored speech at {result:?}");
        assert_eq!(result.is_some(), env("YM_EXPECT_RESTORED") == "yes");
    }

    #[test]
    #[ignore = "requires a real external recording downloaded by the authenticated service"]
    fn external_recording_pair() {
        let dir = std::path::PathBuf::from(std::env::var_os("YM_VERIFICATION_FIXTURES").unwrap());
        let read = |path: std::path::PathBuf| Recording::decode(
            rodio::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap())).unwrap(), 174000).unwrap();
        let baseline = read(dir.join("groupie-ym.mp3"));
        let alternative = read(std::env::var_os("YM_EXTERNAL_AUDIO").unwrap().into());
        let lyrics = Lyrics { synced: crate::api::parse_lrc(&std::fs::read_to_string(dir.join("groupie.lrc")).unwrap()), ..Default::default() };
        assert_eq!(compare(&baseline, &baseline, &lyrics), None);
        assert_eq!(compare(&baseline, &read(dir.join("groupie-recoded.mp3")), &lyrics), None);
        assert_eq!(compare(&alternative, &baseline, &lyrics), None);
        if let Some(path) = std::env::var_os("YM_EXTERNAL_UNCONFIRMED") {
            assert_eq!(compare(&baseline, &read(path.into()), &lyrics), None);
        }
        let restored = compare(&baseline, &alternative, &lyrics);
        eprintln!("external recording: restored speech at {restored:?}");
        assert!(restored.is_some());
    }

    #[test]
    #[ignore = "local recording pairs in YM_VERIFICATION_FIXTURES"]
    fn real_recording_pairs() {
        let dir = std::path::PathBuf::from(std::env::var_os("YM_VERIFICATION_FIXTURES").unwrap());
        for (name, ms) in [("groupie", 174000), ("santa", 121900)] {
            let read = |suffix| {
                let file = std::fs::File::open(dir.join(format!("{name}-{suffix}"))).unwrap();
                Recording::decode(
                    rodio::Decoder::new(std::io::BufReader::new(file)).unwrap(),
                    ms,
                )
                .unwrap()
            };
            let lyrics = Lyrics {
                synced: crate::api::parse_lrc(
                    &std::fs::read_to_string(dir.join(format!("{name}.lrc"))).unwrap(),
                ),
                ..Default::default()
            };
            let baseline = read("ym.mp3");
            assert_eq!(compare(&baseline, &baseline, &lyrics), None);
            assert_eq!(compare(&baseline, &read("recoded.mp3"), &lyrics), None);
            let alternative = read("alt.mp3");
            assert_eq!(
                compare(&alternative, &baseline, &lyrics),
                None,
                "reverse direction: {name}"
            );
            let time = compare(&baseline, &alternative, &lyrics);
            eprintln!("{name}: restored speech at {time:?}");
            assert!(time.is_some(), "{name}");
        }
    }

    #[test]
    fn decodes_gapless_mpeg2_with_empty_first_packet() {
        use std::io::Cursor;

        // A 4.25 s synthetic 440 Hz tone, LAME MP3, 16 kHz mono with gapless metadata.
        let bytes = include_bytes!("../tests/fixtures/gapless-mono-16k.mp3");
        let decoder = || rodio::Decoder::new(Cursor::new(&bytes[..])).unwrap();
        let expected: Vec<i16> = decoder().collect();
        assert!(expected.len().abs_diff(68_000) <= 16);
        let recording = Recording::decode(decoder(), 4250).unwrap();
        assert_eq!(recording.pcm, expected);
    }
}
