//! Candidate-only speech assistance. Requests contain times and a delivered-media
//! receipt, never PCM, source URLs, lyrics, or credentials for the music catalog.
use std::{cell::RefCell, collections::HashMap, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub(crate) struct Word { pub text: String, pub start: f64, pub end: f64 }
#[derive(Clone, Deserialize)]
pub(crate) struct Transcript {
    pub from_ms: u32, pub to_ms: u32, pub text: String, pub words: Vec<Word>,
}
type Range = (u32, u32);
type Query = dyn Fn(&[Range], std::time::Duration) -> Option<Vec<Transcript>> + Send + Sync;
pub(crate) struct Provider {
    query: Box<Query>,
    cache: Mutex<HashMap<Range, Transcript>>,
    unavailable: AtomicBool,
    transient_failure: AtomicBool,
    budget: Mutex<Budget>,
}
#[derive(Default)]
struct Budget {
    started: Option<std::time::Instant>,
    waiting: std::time::Duration,
}
impl Budget {
    fn remaining(&mut self) -> std::time::Duration {
        use std::time::{Duration,Instant};
        let elapsed = self.started.get_or_insert_with(Instant::now).elapsed();
        // Bound network waiting independently from local recognition. Otherwise
        // useful later cues lose their budget while Whisper checks the original.
        Duration::from_secs(4).saturating_sub(self.waiting)
            .min(Duration::from_secs(6).saturating_sub(elapsed))
    }
}
impl Provider {
    pub(crate) fn new(query: impl Fn(&[Range], std::time::Duration) -> Option<Vec<Transcript>> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { query: Box::new(query), cache: Mutex::default(), unavailable: AtomicBool::new(false), transient_failure: AtomicBool::new(false), budget: Mutex::default() })
    }
    fn fetch(&self, ranges: &[Range]) {
        if self.unavailable.load(Ordering::Acquire) { return; }
        let mut cache = self.cache.lock().unwrap();
        let mut missing = Vec::new();
        let mut duration = 0;
        for &range in ranges {
            if cache.contains_key(&range) || missing.contains(&range) { continue; }
            if missing.len() == 4 || duration + range.1 - range.0 > 32_000 { break; }
            duration += range.1 - range.0;
            missing.push(range);
        }
        if missing.is_empty() { return; }
        let mut budget = self.budget.lock().unwrap();
        let remaining = budget.remaining();
        if remaining < std::time::Duration::from_millis(100) {
            self.unavailable.store(true, Ordering::Release); return;
        }
        let started = std::time::Instant::now();
        let results = (self.query)(&missing, remaining.min(std::time::Duration::from_secs(3)));
        budget.waiting += started.elapsed();
        drop(budget);
        let Some(results) = results else {
            self.transient_failure.store(true, Ordering::Release);
            self.unavailable.store(true, Ordering::Release); return;
        };
        let valid = results.len() == missing.len() && results.iter().zip(&missing).all(|(item, &(from,to))| {
            item.from_ms == from && item.to_ms == to && item.text.len() <= 16_384 && item.words.len() <= 256
                && item.words.iter().all(|w| !w.text.is_empty() && w.text.len() <= 800
                    && w.start.is_finite() && w.end.is_finite() && w.start >= 0. && w.end >= w.start
                    && w.end <= (to - from) as f64 / 1000. + 0.1)
        });
        if !valid {
            self.transient_failure.store(true, Ordering::Release);
            self.unavailable.store(true, Ordering::Release); return;
        }
        for result in results { cache.insert((result.from_ms,result.to_ms),result); }
    }
}

#[derive(Clone)]
struct Active { from: usize, to: usize, provider: Arc<Provider> }
thread_local! { static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) }; }
// This scope stays on the verification thread and restores any outer scope even
// during unwinding. The slice is kept borrowed by with_candidate for its lifetime.
struct Scope { previous: Option<Active>, _thread: std::marker::PhantomData<std::rc::Rc<()>> }
impl Drop for Scope {
    fn drop(&mut self) { ACTIVE.with(|slot| { *slot.borrow_mut() = self.previous.take(); }); }
}
pub(crate) fn with_candidate<T>(pcm: &[i16], provider: Arc<Provider>, run: impl FnOnce() -> T) -> T {
    let from = pcm.as_ptr() as usize;
    let Some(to) = from.checked_add(std::mem::size_of_val(pcm)) else { return run(); };
    let previous = ACTIVE.with(|slot| slot.replace(Some(Active { from,to,provider })));
    let _scope = Scope { previous, _thread: std::marker::PhantomData };
    run()
}
// Failed assistance makes a negative result inconclusive. A deliberate time
// budget expiry is also inconclusive, but must not double work by scheduling a retry.
pub(crate) fn incomplete() -> bool {
    ACTIVE.with(|slot| slot.borrow().as_ref().is_some_and(|active|
        active.provider.unavailable.load(Ordering::Acquire)))
}
pub(crate) fn retryable_failure() -> bool {
    ACTIVE.with(|slot| slot.borrow().as_ref().is_some_and(|active|
        active.provider.transient_failure.load(Ordering::Acquire)))
}
fn locate(pcm: &[i16]) -> Option<(Arc<Provider>,Range)> {
    ACTIVE.with(|slot| {
        let active = slot.borrow(); let active = active.as_ref()?;
        let start = pcm.as_ptr() as usize;
        let end = start.checked_add(std::mem::size_of_val(pcm))?;
        if start < active.from || end > active.to || pcm.len() < 320 || pcm.len() > 192_000 { return None; }
        let from = u32::try_from((start - active.from) / 32).ok()?;
        let to = from.checked_add(u32::try_from(pcm.len()/16).ok()?)?;
        Some((active.provider.clone(),(from,to)))
    })
}
pub(crate) fn owns(pcm: &[i16]) -> bool { locate(pcm).is_some() }
pub(crate) fn prefetch<'a>(clips: impl IntoIterator<Item=&'a [i16]>) {
    let mut provider = None; let mut ranges = Vec::new();
    for clip in clips {
        if let Some((p,range)) = locate(clip) { provider = Some(p); ranges.push(range); }
    }
    if let Some(provider) = provider { provider.fetch(&ranges); }
}
// Inspect the provenance of an already decoded clip without starting another request.
pub(crate) fn has_transcript(pcm: &[i16]) -> bool {
    let Some((provider, range)) = locate(pcm) else { return false; };
    let found = provider.cache.lock().unwrap().contains_key(&range);
    found
}
pub(crate) fn transcript(pcm: &[i16]) -> Option<Transcript> {
    let (provider,range) = locate(pcm)?;
    provider.fetch(&[range]);
    let result = provider.cache.lock().unwrap().get(&range).cloned();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    fn answer(ranges: &[Range]) -> Option<Vec<Transcript>> {
        Some(ranges.iter().map(|&(from_ms,to_ms)| Transcript {from_ms,to_ms,text:"word".into(),words:vec![]}).collect())
    }
    #[test] fn only_candidate_ranges_leave_scope_and_duplicate_calls_are_cached() {
        let candidate=vec![0i16;16000*8]; let baseline=vec![0i16;16000*8];
        let calls=Arc::new(AtomicUsize::new(0)); let seen=calls.clone();
        let provider=Provider::new(move |ranges,_timeout| { seen.fetch_add(1,Ordering::Relaxed); assert_eq!(ranges,&[(1000,3000)]); answer(ranges) });
        with_candidate(&candidate,provider,|| {
            assert!(transcript(&baseline[16000..48000]).is_none());
            assert!(transcript(&candidate[16000..48000]).is_some());
            assert!(transcript(&candidate[16000..48000]).is_some());
        });
        assert!(transcript(&candidate[16000..48000]).is_none());
        assert_eq!(calls.load(Ordering::Relaxed),1);
    }
    #[test] fn prefetched_cue_pair_excludes_original_audio_and_reuses_both_results() {
        let candidate=vec![0i16;16000*8]; let baseline=vec![0i16;16000*8];
        let calls=Arc::new(AtomicUsize::new(0)); let seen=calls.clone();
        let provider=Provider::new(move |ranges,_| {
            seen.fetch_add(1,Ordering::Relaxed);
            assert_eq!(ranges,&[(1000,3000),(4000,6000)]);
            answer(ranges)
        });
        with_candidate(&candidate,provider,|| {
            prefetch([&candidate[16000..48000],&baseline[16000..48000],
                &candidate[64000..96000],&baseline[64000..96000]]);
            assert!(transcript(&candidate[16000..48000]).is_some());
            assert!(transcript(&candidate[64000..96000]).is_some());
            assert!(transcript(&baseline[16000..48000]).is_none());
        });
        assert_eq!(calls.load(Ordering::Relaxed),1);
    }
    #[test] fn local_recognition_does_not_consume_the_remaining_network_budget() {
        use std::time::{Duration,Instant};
        let candidate=vec![0i16;16000*4];
        let provider=Provider::new(|ranges,timeout| {
            // Four seconds elapsed locally, but only two were spent waiting.
            assert!(timeout > Duration::from_millis(1800));
            assert!(timeout <= Duration::from_secs(2));
            answer(ranges)
        });
        *provider.budget.lock().unwrap()=Budget {
            started:Some(Instant::now()-Duration::from_secs(4)),waiting:Duration::from_secs(2),
        };
        with_candidate(&candidate,provider,|| assert!(transcript(&candidate[..16000]).is_some()));
    }
    #[test] fn network_spending_and_wall_deadline_each_stop_later_requests() {
        use std::time::{Duration,Instant};
        for (elapsed,waiting) in [(1,4),(6,0)] {
            let candidate=vec![0i16;16000];
            let provider=Provider::new(|_,_| panic!("exhausted budget must not request another clip"));
            *provider.budget.lock().unwrap()=Budget {
                started:Some(Instant::now()-Duration::from_secs(elapsed)),waiting:Duration::from_secs(waiting),
            };
            with_candidate(&candidate,provider,|| {
                assert!(transcript(&candidate).is_none());
                assert!(incomplete());
                assert!(!retryable_failure(), "budget expiry must not schedule more work");
            });
        }
    }
    #[test] fn worker_failure_and_wrong_offsets_disable_further_requests() {
        let candidate=vec![0i16;16000*8];
        for malformed in [false,true] {
            let calls=Arc::new(AtomicUsize::new(0)); let seen=calls.clone();
            let provider=Provider::new(move |ranges,_timeout| { seen.fetch_add(1,Ordering::Relaxed); if malformed { answer(&[(ranges[0].0+1,ranges[0].1)]) } else { None } });
            with_candidate(&candidate,provider,|| {
                assert!(transcript(&candidate[..16000]).is_none());
                assert!(transcript(&candidate[16000..48000]).is_none());
                assert!(incomplete());
                assert!(retryable_failure());
            });
            assert_eq!(calls.load(Ordering::Relaxed),1);
        }
    }
}
