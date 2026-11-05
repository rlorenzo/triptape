//! Bounded file-level parallelism for per-item ingest work.

/// Run `work` over `items` on at most `workers` threads (clamped to
/// `1..=items.len()`), so at most that many items are in flight at once.
///
/// `on_done` runs on the calling thread only, once per item in completion
/// order, as `(input index, result, items done, total)`, so callers can
/// persist and report progress without extra locking.
pub fn for_each_parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    work: impl Fn(&T) -> R + Sync,
    mut on_done: impl FnMut(usize, R, usize, usize),
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let total = items.len();
    if total == 0 {
        return;
    }
    let live = workers.clamp(1, total);
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, R)>();
    std::thread::scope(|s| {
        // Moved into each worker as shared borrows of the same counter and job.
        let (next, work) = (&next, &work);
        for _ in 0..live {
            let tx = tx.clone();
            s.spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::SeqCst);
                let Some(item) = items.get(idx) else { break };
                if tx.send((idx, work(item))).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for (done, (idx, result)) in rx.into_iter().enumerate() {
            on_done(idx, result, done + 1, total);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn covers_every_item_once_and_counts_completions() {
        let items: Vec<usize> = (0..50).collect();
        for workers in [0, 1, 4, 500] {
            let mut seen = vec![];
            let mut dones = vec![];
            for_each_parallel(
                &items,
                workers,
                |n| n * 2,
                |idx, doubled, done, total| {
                    assert_eq!(total, items.len());
                    assert_eq!(doubled, items[idx] * 2);
                    seen.push(idx);
                    dones.push(done);
                },
            );
            seen.sort_unstable();
            assert_eq!(seen, items);
            assert_eq!(dones, (1..=items.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn empty_input_never_calls_back() {
        let mut calls = 0;
        for_each_parallel(&[] as &[u8], 4, |_| (), |_, _, _, _| calls += 1);
        assert_eq!(calls, 0);
    }

    #[test]
    fn never_exceeds_the_worker_bound() {
        let items = vec![(); 32];
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        for_each_parallel(
            &items,
            3,
            |_| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
            },
            |_, _, _, _| {},
        );
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }
}
