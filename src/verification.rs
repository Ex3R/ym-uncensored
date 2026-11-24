//! Verify the audible change before handing a replacement to the mixer.
use crate::api::Lyrics;
use rodio::{source::UniformSourceIterator, Source};
use whisper_rs::{FullParams, SamplingStrategy};

const RATE: usize = 16_000;

const MAX_SECONDS: usize = 12 * 60;

pub(crate) struct Recording {
    pcm: Vec<i16>,
    envelope: Vec<f32>,
    pub digest: [u8; 32],
}

impl Recording {
    pub fn decode(source: impl Source<Item = i16>, duration_ms: u64) -> Option<Self> {
        use sha2::{Digest, Sha256};
        if duration_ms > MAX_SECONDS as u64 * 1000 {
            return None;
        }
        let limit = (duration_ms as usize / 1000 + 4) * RATE;
        let mut stereo = UniformSourceIterator::<_, i16>::new(source, 2, RATE as u32);
        let pcm: Vec<i16> = std::iter::from_fn(|| {
            Some(((stereo.next()? as i32 + stereo.next()? as i32) / 2) as i16)
        })
        .take(limit)
        .collect();
        if (pcm.len() / 16).abs_diff(duration_ms as usize) > 2200 || pcm.len() < 4 * RATE {
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
            digest: hash.finalize().into(),
        })
    }
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
    text.to_lowercase()
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

fn transcript(pcm: &[i16], model: &whisper_rs::WhisperContext) -> Option<Vec<String>> {
    let mut state = model.create_state().ok()?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("ru"));
    params.set_n_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as i32);
    params.set_no_context(true);
    params.set_temperature_inc(0.);
    params.set_max_tokens(48);
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

// Rank sustained energy added in the speech band after compensating each bin for mastering gain.
fn added_speech_windows(before: &[i16], after: &[i16]) -> Vec<(f32, usize)> {
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
    let mut scores: Vec<_> = added
        .windows(28)
        .step_by(5)
        .enumerate()
        .map(|(i, window)| (window.iter().sum::<f32>() / 28., i * 100 + 280))
        .collect();
    scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    scores
}

pub(crate) fn compare(original: &Recording, candidate: &Recording, lyrics: &Lyrics) -> Option<f32> {
    if original.digest == candidate.digest {
        return None;
    }
    let lag = alignment(&original.envelope, &candidate.envelope)?;
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
    let mut windows = Vec::new();
    let mut tried = Vec::new();
    let all_text = lyrics.text.join(" ");
    for (score, offset) in scores {
        if score < 0.14 || tried.len() >= 12 {
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
        windows.push((sec, &original.pcm[begin * 16..end * 16], &candidate.pcm[at * 16..(at + end - begin) * 16], expected));
    }
    type SpeechWindow<'a> = (f32, &'a [i16], &'a [i16], Vec<String>);
    let check = |model: &whisper_rs::WhisperContext, windows: &[SpeechWindow<'_>]| {
        windows.iter().find_map(|(sec, before, after, expected)| {
            let added = transcript(after, model)?;
            if !restored(&[], &added, expected) { return None; }
            let previous = transcript(before, model)?;
            restored(&previous, &added, expected).then_some(*sec)
        })
    };
    let fast = if lyrics.synced.is_empty() { 0 } else { windows.len().min(3) };
    if fast > 0 {
        if let Some(model) = crate::language::whisper_model() {
            if let Some(time) = check(&model, &windows[..fast]) { return Some(time); }
        }
    }
    if let Some(model) = full_speech_model() {
        let model = model.lock().ok()?;
        return check(&model, &windows);
    }
    let model = crate::language::whisper_model()?;
    check(&model, &windows[fast..])
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let lyrics = Lyrics {
            synced: crate::api::parse_lrc(data["syncedLyrics"].as_str().unwrap_or("")),
            text: data["plainLyrics"].as_str().unwrap_or("").lines().map(str::to_owned).collect(),
            ..Default::default()
        };
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
}
