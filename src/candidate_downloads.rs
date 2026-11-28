use crate::api::Res;
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub(super) struct Cursor {
    pub ticket: String,
    pub after_rejection: bool,
}

/// Let delivery and continuation discovery overlap. Only the receiving thread can
/// verify, accept or report a recording. Dropping the receiver stops further work.
pub(super) fn first_verified<C, M, A>(
    first_page: (Vec<C>, Option<Cursor>),
    may_use_cache: impl Fn(&C) -> bool + Send + 'static,
    current: impl Fn() -> bool + Clone + Send + 'static,
    next: impl Fn(&str) -> Res<(Vec<C>, Option<Cursor>)> + Send + 'static,
    fetch: impl Fn(&C, &dyn Fn() -> bool) -> Res<M> + Clone + Send + 'static,
    prepare: impl FnOnce(&dyn Fn() -> bool),
    mut inspect: impl FnMut(Res<(C, M)>) -> Option<A>,
) -> Option<A>
where
    C: Send + 'static,
    M: Send + 'static,
{
    let (first, continuation) = first_page;
    if first.is_empty() || !current() { return None; }
    let done = Arc::new(());
    let owner = Arc::downgrade(&done);
    let job = current.clone();
    let active = move || owner.strong_count() > 0 && job();
    let workers = if first.len() > 1 || continuation.is_some() { 2 } else { 1 };
    let mut pending = first.len();
    let mut pending_cache_hit = first.iter().any(&may_use_cache);
    let (rejected, rejections) = mpsc::channel();
    let (enqueue, candidates) = mpsc::channel();
    for item in first { enqueue.send(item).ok()?; }
    let candidates = Arc::new(Mutex::new(candidates));
    // Back pressure limits each worker to one prepared file while the verifier is busy.
    let (send, receive) = mpsc::sync_channel(0);
    let delivered = Arc::new(AtomicBool::new(false));
    for _ in 0..workers {
        let candidates = candidates.clone();
        let delivered = delivered.clone();
        let (active, fetch, send) = (active.clone(), fetch.clone(), send.clone());
        thread::spawn(move || {
            while active() {
                let next = candidates.lock().unwrap().recv_timeout(Duration::from_millis(100));
                let item = match next {
                    Ok(item) => item,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if !active() { break; }
                let result = fetch(&item, &active).map(|media| (item, media));
                delivered.store(true, Ordering::Release);
                if !active() || send.send(result).is_err() { break; }
            }
        });
    }
    if let Some(mut cursor) = continuation {
        thread::spawn(move || {
            // A ready local copy may finish before another source search is needed.
            // The service has a finite set of stages. Cap pagination as a safeguard
            // against a broken endpoint returning a fresh continuation indefinitely.
            for _ in 0..4 {
                // More discovery can launch several external processes. Reserve it for
                // a free download slot, counting files still awaiting acoustic proof.
                // Some cursors only reveal known fallback recordings. Opening those
                // before every offered candidate is rejected needlessly downloads audio.
                let limit = if cursor.after_rejection { 1 } else { workers };
                while pending >= limit {
                    if !active() { return; }
                    match rejections.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) => pending -= 1,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                // Cold media still needs a provider transfer. A free slot can start
                // the next search immediately. Give a possible cache hit time to win.
                if pending > 0 && pending_cache_hit {
                    match rejections.recv_timeout(Duration::from_millis(300)) {
                        Ok(()) => pending -= 1,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                if !active() { return; }
                match next(&cursor.ticket) {
                    Ok((items, follow)) => {
                        pending_cache_hit = (pending > 0 && pending_cache_hit) || items.iter().any(&may_use_cache);
                        pending += items.len();
                        for item in items {
                            if !active() || enqueue.send(item).is_err() { return; }
                        }
                        match follow {
                            Some(page) if page.ticket != cursor.ticket => cursor = page,
                            _ => return,
                        }
                    }
                    Err(error) => { let _ = send.send(Err(error)); return; }
                }
            }
        });
    } else {
        drop(enqueue);
        drop(send);
    }
    prepare(&|| current() && !delivered.load(Ordering::Acquire));
    while current() {
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                if !current() { break; }
                if let Some(verified) = inspect(result) { return Some(verified); }
                let _ = rejected.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cursor(ticket: &str, after_rejection: bool) -> Option<Cursor> {
        Some(Cursor { ticket: ticket.into(), after_rejection })
    }

    #[test]
    fn preparation_overlaps_delivery_and_stops_as_soon_as_media_arrives() {
        let (started, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let hit = first_verified((vec![7], None), |_| true, || true, |_| panic!("no continuation"),
            move |item, _| {
                wait.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(*item)
            },
            |waiting| {
                assert!(waiting());
                started.send(()).unwrap();
                let limit = std::time::Instant::now() + Duration::from_secs(2);
                while waiting() && std::time::Instant::now() < limit { thread::yield_now(); }
                assert!(!waiting());
            },
            |result| result.ok().map(|(_, v)| v));
        assert_eq!(hit, Some(7));
    }

    #[test]
    fn verified_continuation_can_win_before_first_transfer_finishes() {
        let (started, first_started) = mpsc::channel();
        let (ended, first_ended) = mpsc::channel();
        let result = first_verified((vec![1], cursor("ticket", false)), |_| true, || true,
            move |ticket| {
                assert_eq!(ticket, "ticket");
                first_started.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok((vec![2, 3], None))
            },
            move |item, active| {
                if *item == 1 {
                    started.send(()).unwrap();
                    while active() { thread::sleep(Duration::from_millis(5)); }
                    ended.send(()).unwrap();
                }
                Ok(*item)
            },
            |_| {},
            |result| {
                let (item, bytes) = result.unwrap();
                (item == 3 && bytes == 3).then_some(item)
            });
        assert_eq!(result, Some(3));
        first_ended.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn a_full_first_page_holds_discovery_until_a_file_is_rejected() {
        let rejected = Arc::new(AtomicBool::new(false));
        let seen = rejected.clone();
        let result = first_verified((vec![1, 2], cursor("remaining", false)), |_| true, || true,
            move |_| {
                assert!(seen.load(Ordering::Acquire));
                Ok((vec![3], None))
            },
            |item, _| {
                if *item < 3 { thread::sleep(Duration::from_millis(450)); }
                Ok(*item)
            },
            |_| {},
            |response| {
                let (_, item) = response.ok()?;
                if item == 3 { return Some(item); }
                rejected.store(true, Ordering::Release);
                None
            });
        assert_eq!(result, Some(3));
    }

    #[test]
    fn candidates_on_a_later_page_still_get_verified() {
        let result = first_verified((vec![1], cursor("regional", false)), |_| true, || true,
            |ticket| match ticket {
                "regional" => Ok((vec![2], cursor("remaining", false))),
                "remaining" => Ok((vec![3], None)),
                _ => panic!("unexpected continuation"),
            },
            |item, _| Ok(*item),
            |_| {},
            |result| result.ok().map(|(_, media)| media).filter(|&media| media == 3));
        assert_eq!(result, Some(3));
    }

    #[test]
    fn cancelled_playback_cannot_accept_or_report_an_in_flight_candidate() {
        let playing = Arc::new(AtomicBool::new(true));
        let state = playing.clone();
        let accepted = first_verified((vec![1], None), |_| true, move || state.load(Ordering::Acquire),
            |_| panic!("no continuation"),
            move |_, _| { playing.store(false, Ordering::Release); Ok(()) },
            |_| {},
            |_| -> Option<()> { panic!("stale candidate reached verifier") });
        assert_eq!(accepted, None);
    }

    #[test]
    fn accepting_cached_media_skips_continuation_discovery() {
        let (sent, calls) = mpsc::channel();
        let hit = first_verified((vec![1], cursor("next", false)), |_| true, || true,
            move |_| { sent.send(()).unwrap(); Ok((vec![], None)) },
            |item, _| Ok(*item),
            |_| {},
            |result| result.ok().map(|(_, media)| media));
        assert_eq!(hit, Some(1));
        assert_eq!(calls.recv_timeout(Duration::from_secs(2)), Err(mpsc::RecvTimeoutError::Disconnected));
    }

    #[test]
    fn saved_fallback_waits_for_all_verdicts_even_after_the_cache_grace() {
        for cached in [true,false] {
            let (sent,calls) = mpsc::channel();
            let (rejected,wait) = mpsc::channel();
            let wait = Arc::new(Mutex::new(wait));
            let outcome = first_verified((vec![1,2],cursor("saved",true)), move |_| cached, || true,
                move |_| { let _ = sent.send(()); Ok((vec![3],None)) },
                move |item,_| {
                    if *item == 2 { wait.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap(); }
                    Ok(*item)
                }, |_| {}, |incoming| {
                    let (item,_) = incoming.unwrap();
                    if item == 1 { rejected.send(()).unwrap(); return None; }
                    // Let the pagination worker observe the free slot after one rejection.
                    thread::sleep(Duration::from_millis(400));
                    Some(item)
                });
            assert_eq!(outcome,Some(2));
            assert_eq!(calls.recv_timeout(Duration::from_secs(2)),Err(mpsc::RecvTimeoutError::Disconnected));
        }
    }

    #[test]
    fn a_deferred_cursor_on_a_later_page_resumes_after_a_delivery_error() {
        let rejected = Arc::new(AtomicBool::new(false));
        let seen = rejected.clone();
        let result = first_verified((vec![1],cursor("normal",false)), |_| false, || true,
            move |ticket| match ticket {
                "normal" => Ok((vec![2],cursor("saved",true))),
                "saved" => { assert!(seen.load(Ordering::Acquire)); Ok((vec![3],None)) },
                _ => panic!("unknown cursor"),
            },
            |item,_| if *item == 2 { Err("missing".to_owned()) } else { Ok(*item) }, |_| {},
            |response| match response {
                Err(_) => { rejected.store(true,Ordering::Release); None }
                Ok((item,_)) => (item == 3).then_some(item),
            });
        assert_eq!(result,Some(3));
    }

    #[test]
    fn cancellation_before_a_verdict_leaves_the_deferred_cursor_unused() {
        let current = Arc::new(AtomicBool::new(true));
        let playing = current.clone();
        let (sent,calls) = mpsc::channel();
        let result = first_verified((vec![1],cursor("saved",true)), |_| false, move || playing.load(Ordering::Acquire),
            move |_| { let _ = sent.send(()); Ok((vec![2],None)) },
            move |item,_| { current.store(false,Ordering::Release); Ok(*item) }, |_| {}, |_| -> Option<i32> { panic!("stale audio") });
        assert_eq!(result,None);
        assert_eq!(calls.recv_timeout(Duration::from_secs(2)),Err(mpsc::RecvTimeoutError::Disconnected));
    }

    #[test]
    fn two_initial_files_can_load_concurrently_without_waiting_for_more_sources() {
        let both = Arc::new(std::sync::Barrier::new(2));
        let result = first_verified((vec![1, 2], None), |_| true, || true, |_| panic!("no continuation"),
            move |item, _| { both.wait(); Ok(*item) },
            |_| {},
            |response| { let (_, item) = response.unwrap(); (item == 2).then_some(item) });
        assert_eq!(result, Some(2));
    }
}
