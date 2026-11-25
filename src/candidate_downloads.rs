use crate::api::Res;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// Let delivery and continuation discovery overlap. Only the receiving thread can
/// verify, accept or report a recording. Dropping the receiver stops further work.
pub(super) fn first_verified<C, M, A>(
    first: Vec<C>,
    continuation: Option<String>,
    current: impl Fn() -> bool + Clone + Send + 'static,
    next: impl FnOnce(&str) -> Res<Vec<C>> + Send + 'static,
    fetch: impl Fn(&C, &dyn Fn() -> bool) -> Res<M> + Clone + Send + 'static,
    mut inspect: impl FnMut(Res<(C, M)>) -> Option<A>,
) -> Option<A>
where
    C: Send + 'static,
    M: Send + 'static,
{
    if first.is_empty() || !current() { return None; }
    let done = Arc::new(());
    let owner = Arc::downgrade(&done);
    let job = current.clone();
    let active = move || owner.strong_count() > 0 && job();
    let workers = if first.len() > 1 || continuation.is_some() { 2 } else { 1 };
    let (enqueue, candidates) = mpsc::channel();
    for item in first { enqueue.send(item).ok()?; }
    let candidates = Arc::new(Mutex::new(candidates));
    // Back pressure limits each worker to one prepared file while the verifier is busy.
    let (send, receive) = mpsc::sync_channel(0);
    for _ in 0..workers {
        let candidates = candidates.clone();
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
                if !active() || send.send(result).is_err() { break; }
            }
        });
    }
    if let Some(ticket) = continuation {
        thread::spawn(move || {
            // Give a ready local copy a chance to win without asking for more sources.
            thread::sleep(Duration::from_millis(300));
            if !active() { return; }
            match next(&ticket) {
                Ok(items) => {
                    for item in items {
                        if !active() || enqueue.send(item).is_err() { break; }
                    }
                }
                Err(error) => { let _ = send.send(Err(error)); }
            }
        });
    } else {
        drop(enqueue);
        drop(send);
    }
    while current() {
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                if !current() { break; }
                if let Some(verified) = inspect(result) { return Some(verified); }
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

    #[test]
    fn verified_continuation_can_win_before_first_transfer_finishes() {
        let (started, first_started) = mpsc::channel();
        let (ended, first_ended) = mpsc::channel();
        let result = first_verified(vec![1], Some("ticket".into()), || true,
            move |ticket| {
                assert_eq!(ticket, "ticket");
                first_started.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(vec![2, 3])
            },
            move |item, active| {
                if *item == 1 {
                    started.send(()).unwrap();
                    while active() { thread::sleep(Duration::from_millis(5)); }
                    ended.send(()).unwrap();
                }
                Ok(*item)
            },
            |result| {
                let (item, bytes) = result.unwrap();
                (item == 3 && bytes == 3).then_some(item)
            });
        assert_eq!(result, Some(3));
        first_ended.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn cancelled_playback_cannot_accept_or_report_an_in_flight_candidate() {
        let playing = Arc::new(AtomicBool::new(true));
        let state = playing.clone();
        let accepted = first_verified(vec![1], None, move || state.load(Ordering::Acquire),
            |_| panic!("no continuation"),
            move |_, _| { playing.store(false, Ordering::Release); Ok(()) },
            |_| -> Option<()> { panic!("stale candidate reached verifier") });
        assert_eq!(accepted, None);
    }

    #[test]
    fn two_initial_files_can_load_concurrently_without_waiting_for_more_sources() {
        let both = Arc::new(std::sync::Barrier::new(2));
        let result = first_verified(vec![1, 2], None, || true, |_| panic!("no continuation"),
            move |item, _| { both.wait(); Ok(*item) },
            |response| { let (_, item) = response.unwrap(); (item == 2).then_some(item) });
        assert_eq!(result, Some(2));
    }
}
