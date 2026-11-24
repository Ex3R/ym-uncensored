use crate::api::Lyrics;
use rodio::{source::UniformSourceIterator, Source};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use whatlang::{detect, Lang};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn cyrillic(c: char) -> bool {
    ('\u{0400}'..='\u{052f}').contains(&c)
}

fn russian_letters(word: &str) -> bool {
    word.chars().all(|c| ('а'..='я').contains(&c) || c == 'ё')
}

fn headwords() -> &'static HashSet<&'static str> {
    static TEXT: OnceLock<String> = OnceLock::new();
    static WORDS: OnceLock<HashSet<&str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let text = TEXT.get_or_init(|| {
            let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(include_bytes!("../models/ru-headwords.zlib"))
                .expect("bundled Russian dictionary");
            String::from_utf8(bytes).expect("UTF-8 Russian dictionary")
        });
        text.lines().collect()
    })
}

pub fn russian_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    let words: Vec<_> = lower.split(|c: char| !c.is_alphabetic())
        .filter(|word| word.chars().any(cyrillic)).collect();
    if words.is_empty() || !words.iter().all(|word| russian_letters(word)) { return false; }
    let text = words.join(" ");
    let Some(candidate) = detect(&text) else { return false };
    if candidate.lang() != Lang::Rus && candidate.confidence() > 0.35 { return false; }
    let distinctive = words.iter().any(|word| headwords().contains(*word));
    distinctive || (candidate.lang() == Lang::Rus && candidate.confidence() >= 0.15)
}

pub fn lyrics_language(lyrics: &Lyrics) -> Option<bool> {
    // Ignore section labels and attribution. Check individual lines for bilingual songs.
    let mut text = String::new();
    for line in lyrics.text.iter().chain(lyrics.synced.iter().map(|line| &line.text)) {
        let line = line.trim();
        if line.is_empty() || (line.starts_with('[') && line.ends_with(']'))
            || (line.starts_with('(') && line.ends_with(')')) { continue; }
        if line.chars().filter(|c| cyrillic(*c)).count() >= 12
            && detect(line).is_some_and(|lang| lang.lang() == Lang::Rus && lang.confidence() >= 0.25)
        {
            return Some(true);
        }
        text.push_str(line);
        text.push(' ');
    }
    if text.chars().filter(|c| c.is_alphabetic()).count() < 12 { return None; }
    let lang = detect(&text)?;
    if !text.chars().any(cyrillic) { return lang.is_reliable().then_some(false); }
    (lang.confidence() >= 0.25).then_some(lang.lang() == Lang::Rus)
}

pub(crate) fn whisper_model() -> Option<std::sync::MutexGuard<'static, WhisperContext>> {
    static MODEL: OnceLock<Option<Mutex<WhisperContext>>> = OnceLock::new();
    MODEL.get_or_init(|| {
        whisper_rs::install_logging_hooks();
        WhisperContext::new_from_buffer_with_params(include_bytes!("../models/ggml-tiny-q5_1.bin"),
            WhisperContextParameters::default()).ok().map(Mutex::new)
    }).as_ref()?.lock().ok()
}

pub fn vocal_language(mut audio: impl Source<Item = i16>, duration_ms: u64) -> Option<bool> {
    // Sample beyond the intro. The decoder for playback keeps its own untouched cursor.
    const RATE: usize = 16_000;
    let offset = (duration_ms / 4).min(30_000);
    let seeked = audio.try_seek(Duration::from_millis(offset)).is_ok();
    let pcm: Vec<f32> = UniformSourceIterator::new(audio, 1, RATE as u32)
        .skip(if seeked { 0 } else { offset as usize * RATE / 1000 })
        .take(16 * RATE).collect();
    if pcm.len() < 2 * RATE || pcm.iter().all(|s| s.abs() < 0.001) { return None; }

    let model = whisper_model()?;
    let mut state = model.create_state().ok()?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    state.pcm_to_mel(&pcm, threads).ok()?;
    let (id, probabilities) = state.lang_detect(0, threads).ok()?;
    if *probabilities.get(id as usize)? < 0.78 { return None; }
    if whisper_rs::get_lang_str(id)? != "ru" { return Some(false); }

    // Music alone can produce a language guess. Require intelligible words as well.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("ru"));
    params.set_n_threads(threads as i32);
    params.set_translate(false);
    params.set_temperature_inc(0.0);
    params.set_print_progress(false);
    params.set_print_timestamps(false);
    params.set_print_realtime(false);
    params.set_no_context(true);
    state.full(params, &pcm).ok()?;
    let mut words = HashSet::new();
    for segment in state.as_iter().filter(|seg| seg.no_speech_probability() < 0.4) {
        let line = segment.to_str().ok()?.to_lowercase();
        if line.contains("субтитр") || line.contains("спасибо за просмотр") { continue; }
        words.extend(line.split(|c: char| !c.is_alphabetic()).filter(|w| w.len() >= 4 && russian_letters(w)).map(str::to_owned));
    }
    (words.len() >= 3).then_some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_lyrics_use_language_evidence() {
        for title in ["Любовь", "Город под подошвой", "Счастье", "Прощаться"] {
            assert!(russian_title(title), "{title}");
        }
        for title in ["Without Me", "Щастя", "Кохання", "Обійми", "Красива песен", "Паўлінка", "123"] {
            assert!(!russian_title(title), "{title}");
        }
        let lyrics = |lines: &[&str]| Lyrics { text: lines.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        assert_eq!(lyrics_language(&lyrics(&["[Припев]", "We'll meet at the river when the night is over"])), Some(false));
        assert_eq!(lyrics_language(&lyrics(&["We'll meet at the river", "Мы вернёмся домой, когда погаснут огни"])), Some(true));
        assert_eq!(lyrics_language(&lyrics(&["[Инструментал]"])), None);
    }

    #[test]
    #[ignore = "requires 16 kHz mono PCM recordings in YM_TEST_VOCALS"]
    fn speech_and_music_samples() {
        let root = std::path::PathBuf::from(std::env::var_os("YM_TEST_VOCALS").expect("YM_TEST_VOCALS"));
        for file in std::fs::read_dir(root).unwrap().flatten() {
            let path = file.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pcm") { continue; }
            let bytes = std::fs::read(&path).unwrap();
            let samples: Vec<i16> = bytes.chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]])).collect();
            let start = std::time::Instant::now();
            let audio = rodio::buffer::SamplesBuffer::new(1, 16_000, samples);
            let result = vocal_language(audio, 0);
            let name = path.file_stem().unwrap().to_string_lossy();
            eprintln!("{name} {result:?} {:?}", start.elapsed());
            if name.starts_with("russian") { assert_eq!(result, Some(true), "{name}"); }
            else { assert_ne!(result, Some(true), "{name}"); }
        }
    }
}
