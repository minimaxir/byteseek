//! Maps chunks of one input on the rayon pool, with the calling thread taking part.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

thread_local! {
    /// Whether this non-worker thread is currently draining a queue.
    static CALLER_IS_DRAINING: Cell<bool> = const { Cell::new(false) };
}

/// Chunks handed out in order to whichever threads ask, with a result slot per chunk.
struct ChunkQueue<'m, R, M> {
    map: &'m M,
    /// Each slot is written once, by the thread that maps its chunk, and taken by the caller,
    /// which polls slots in order when it merges early. The locks only make `R: Send` enough,
    /// rather than `Sync`; a single lock would be contended when all helpers finish at once.
    results: Vec<Mutex<Option<R>>>,
    /// The next unclaimed chunk.
    next: AtomicUsize,
    /// Helper tasks spawned so far, and the cap.
    helpers: AtomicUsize,
    max_helpers: usize,
    /// Spawned helper tasks that have not finished.
    unfinished: AtomicUsize,
}

/// Guard run when a thread leaves a queue, including by panic.
struct Leave<'q> {
    next: &'q AtomicUsize,
    chunk_count: usize,
    /// The unfinished-helper count, if this is a helper.
    unfinished: Option<&'q AtomicUsize>,
}

impl Drop for Leave<'_> {
    fn drop(&mut self) {
        // A panic fails the whole map, so stop others claiming chunks.
        if std::thread::panicking() {
            self.next.fetch_max(self.chunk_count, Ordering::Relaxed);
        }
        if let Some(unfinished) = self.unfinished {
            // Ends the caller's spin in `await_helpers`, which reads results from their slots.
            unfinished.fetch_sub(1, Ordering::Release);
        }
    }
}

/// What helpers do between chunks.
fn do_nothing() {}

impl<R: Send, M: Fn(usize) -> R + Sync> ChunkQueue<'_, R, M> {
    /// Claims and maps chunks until none is left, running `between` after each. Each claim
    /// that leaves more chunks waiting spawns one more helper, so short inputs wake few
    /// threads. A panic in `map` or `between` stops the other threads claiming chunks.
    fn drain<'scope>(
        &'scope self,
        scope: &rayon::Scope<'scope>,
        unfinished: Option<&'scope AtomicUsize>,
        mut between: impl FnMut(),
    ) {
        let _leave = Leave {
            next: &self.next,
            chunk_count: self.results.len(),
            unfinished,
        };
        loop {
            // `Relaxed` suffices: counters only hand out distinct numbers, and the scope's
            // end orders the results before they are read.
            let index = self.next.fetch_add(1, Ordering::Relaxed);
            if index >= self.results.len() {
                return;
            }
            if index + 1 < self.results.len()
                && self.helpers.load(Ordering::Relaxed) < self.max_helpers
                && self.helpers.fetch_add(1, Ordering::Relaxed) < self.max_helpers
            {
                self.unfinished.fetch_add(1, Ordering::Relaxed);
                // A named function, since a closure here would have a new type in each
                // instance of this one, without end.
                scope.spawn(|scope| self.drain(scope, Some(&self.unfinished), do_nothing));
            }
            let result = (self.map)(index);
            // Poisoning can't leave a slot half written, so it is safe to ignore.
            *self.results[index]
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(result);
            between();
        }
    }
}

impl<R, M> ChunkQueue<'_, R, M> {
    /// Passes `merge` the results from chunk `merged` up to the first one not yet mapped,
    /// advancing `merged` past them.
    fn merge_ready(&self, merged: &mut usize, merge: &mut impl FnMut(usize, R)) {
        while let Some(slot) = self.results.get(*merged) {
            let Some(result) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() else {
                return;
            };
            merge(*merged, result);
            *merged += 1;
        }
    }

    /// Spins until the helpers finish, for at most `patience`, running `meanwhile` between
    /// spins.
    ///
    /// Sleeping and being woken by the last helper costs more than most chunks, so a brief
    /// spin is cheaper. It is bounded by the caller's own work time, since helpers slower
    /// than that are likely descheduled. It doesn't yield, as a syscall would cost more
    /// than the whole wait.
    fn await_helpers(&self, patience: Duration, mut meanwhile: impl FnMut()) {
        /// Reading the clock costs far more than a spin.
        const SPINS_PER_CLOCK_READ: usize = 64;
        /// Many times the tens of microseconds a sleep and wake costs.
        const LONGEST_SPIN: Duration = Duration::from_millis(1);

        let patience = patience.min(LONGEST_SPIN);
        let waiting_since = Instant::now();
        while self.unfinished.load(Ordering::Acquire) != 0 {
            meanwhile();
            for _ in 0..SPINS_PER_CLOCK_READ {
                std::hint::spin_loop();
            }
            if waiting_since.elapsed() > patience {
                return;
            }
        }
    }
}

/// Maps chunk indexes `0..chunk_count` on the current rayon pool, returning results in order.
///
/// The caller starts mapping immediately and pool threads join as they wake, each taking
/// the next unclaimed chunk, so slow cores take fewer. Unlike a parallel iterator, the
/// caller never parks waiting for a worker to wake.
///
/// A nested call from a non-worker thread already mapping chunks runs sequentially, since
/// the workers are busy with the outer call. Use [`map_nested_chunks`] when the outer call
/// leaves threads free.
///
/// # Panics
///
/// Panics if `map` does, after in-flight chunks finish.
pub(crate) fn map_chunks<R: Send>(chunk_count: usize, map: impl Fn(usize) -> R + Sync) -> Vec<R> {
    map_with_helpers::<false, _>(chunk_count, map)
}

/// Like [`map_chunks`], but nested calls also spawn helpers. For tree-shaped work whose
/// outer call bounds its concurrency, leaving threads free for the inner calls.
///
/// # Panics
///
/// Panics if `map` does, after in-flight chunks finish.
pub(crate) fn map_nested_chunks<R: Send>(
    chunk_count: usize,
    map: impl Fn(usize) -> R + Sync,
) -> Vec<R> {
    map_with_helpers::<true, _>(chunk_count, map)
}

/// Shared body of [`map_chunks`] and, with `NESTED_HELPERS`, [`map_nested_chunks`].
fn map_with_helpers<const NESTED_HELPERS: bool, R: Send>(
    chunk_count: usize,
    map: impl Fn(usize) -> R + Sync,
) -> Vec<R> {
    let mut results = Vec::with_capacity(chunk_count);
    merge_with_helpers::<NESTED_HELPERS, false, _>(chunk_count, map, |_, result| {
        results.push(result);
    });
    results
}

/// Maps chunk indexes `0..chunk_count` like [`map_chunks`] and passes each result, with its
/// index, to `merge` on the calling thread, in order.
///
/// The caller merges the results that are ready between the chunks it maps, while the pool's
/// threads map the rest. Joining every thread's output after the last chunk adds a serial
/// pass over all of it; joined meanwhile, most of that costs no wall time.
///
/// # Panics
///
/// Panics if `map` or `merge` does, after in-flight chunks finish.
pub(crate) fn merge_chunks<R: Send>(
    chunk_count: usize,
    map: impl Fn(usize) -> R + Sync,
    merge: impl FnMut(usize, R),
) {
    merge_with_helpers::<false, true, _>(chunk_count, map, merge);
}

/// Maps chunk indexes `0..chunk_count` like [`merge_chunks`] and joins the elements of their
/// results in chunk order, up to `most` elements if given.
///
/// Chunks claimed after the first `most` elements are joined are skipped rather than mapped.
///
/// # Panics
///
/// Panics if `map` does, after in-flight chunks finish.
pub(crate) fn concat_chunks<T: Send>(
    chunk_count: usize,
    most: Option<usize>,
    map: impl Fn(usize) -> Vec<T> + Sync,
) -> Vec<T> {
    let is_full = |joined: &Vec<T>| most.is_some_and(|most| joined.len() >= most);
    let full = AtomicBool::new(most == Some(0));
    let mut joined = Vec::new();
    merge_chunks(
        chunk_count,
        |chunk| {
            if full.load(Ordering::Relaxed) {
                Vec::new()
            } else {
                map(chunk)
            }
        },
        |chunk, mut elements| {
            if chunk == 0 {
                joined = adopt_first_chunk(elements, chunk_count, most);
            } else if !is_full(&joined) {
                joined.append(&mut elements);
            }
            if is_full(&joined) {
                full.store(true, Ordering::Relaxed);
            }
        },
    );
    if let Some(most) = most {
        joined.truncate(most);
    }
    release_unused_capacity(&mut joined);
    joined
}

/// Makes `first`, the result of the first of `chunk_count` chunks, the buffer the other
/// chunks' results are appended to, of at most `most` elements in the end if given.
///
/// Room for the others is reserved at the first chunk's density plus an eighth, so an even
/// input never regrows the buffer.
pub(crate) fn adopt_first_chunk<T>(
    mut first: Vec<T>,
    chunk_count: usize,
    most: Option<usize>,
) -> Vec<T> {
    let others = first.len().saturating_mul(chunk_count.saturating_sub(1));
    reserve_for_other_chunks(&mut first, others, most);
    first
}

/// Reserves room in `merged` for `others` more elements and an eighth again, up to `most`
/// elements in all if given.
///
/// `others` is an estimate, which an uneven input can leave far too high. An allocator that
/// refuses the room is not an error: the buffer then grows as elements arrive.
pub(crate) fn reserve_for_other_chunks<T>(merged: &mut Vec<T>, others: usize, most: Option<usize>) {
    let room = most.map_or(usize::MAX, |most| most.saturating_sub(merged.len()));
    let _ = merged.try_reserve_exact(others.saturating_add(others / 8).min(room));
}

/// Shrinks a finished buffer if most of it is unused, so idle capacity does not pile up.
pub(crate) fn release_unused_capacity<T>(buffer: &mut Vec<T>) {
    if buffer.capacity() / 2 > buffer.len() {
        buffer.shrink_to_fit();
    }
}

/// Shared body of the chunk maps. Unless `MERGE_EARLY`, `merge` only runs once every chunk
/// is mapped, for results that are cheap to collect.
fn merge_with_helpers<const NESTED_HELPERS: bool, const MERGE_EARLY: bool, R: Send>(
    chunk_count: usize,
    map: impl Fn(usize) -> R + Sync,
    mut merge: impl FnMut(usize, R),
) {
    let caller_is_worker = rayon::current_thread_index().is_some();
    // The caller counts as one of the pool's threads even when it isn't one.
    let max_helpers = (rayon::current_num_threads() - 1).min(chunk_count.saturating_sub(1));
    let caller_was_draining = CALLER_IS_DRAINING.get();
    if max_helpers == 0 || (caller_was_draining && !NESTED_HELPERS && !caller_is_worker) {
        (0..chunk_count).for_each(|chunk| merge(chunk, map(chunk)));
        return;
    }
    let queue = ChunkQueue {
        map: &map,
        results: (0..chunk_count).map(|_| Mutex::new(None)).collect(),
        next: AtomicUsize::new(0),
        helpers: AtomicUsize::new(0),
        max_helpers,
        unfinished: AtomicUsize::new(0),
    };
    let mut merged = 0_usize;
    let mut merge_mapped = || {
        if MERGE_EARLY {
            queue.merge_ready(&mut merged, &mut merge);
        }
    };
    // Runs on the calling thread and returns once every spawned task has finished.
    rayon::in_place_scope(|scope| {
        if caller_is_worker {
            // Don't spin: a worker waiting on its scope runs queued tasks, its own helpers
            // among them.
            queue.drain(scope, None, &mut merge_mapped);
            return;
        }
        let started = Instant::now();
        CALLER_IS_DRAINING.set(true);
        // Restored on panic too, via `Drop`.
        let _restore = RestoreDraining(caller_was_draining);
        queue.drain(scope, None, &mut merge_mapped);
        queue.await_helpers(started.elapsed(), &mut merge_mapped);
    });
    queue.merge_ready(&mut merged, &mut merge);
    assert_eq!(
        merged, chunk_count,
        "every chunk is claimed and mapped before the scope ends"
    );
}

/// Restores [`CALLER_IS_DRAINING`] when the caller leaves its queue.
struct RestoreDraining(bool);

impl Drop for RestoreDraining {
    fn drop(&mut self) {
        CALLER_IS_DRAINING.set(self.0);
    }
}

/// Fills `output` in parallel, calling `fill(index, piece)` on pieces of `chunk_len`
/// (nonzero) elements, like [`map_chunks`].
pub(crate) fn fill_chunks<T: Send>(
    output: &mut [T],
    chunk_len: usize,
    fill: impl Fn(usize, &mut [T]) + Sync,
) {
    // Uncontended per-piece locks give shared access to disjoint `&mut` pieces.
    let pieces = output
        .chunks_mut(chunk_len)
        .map(Mutex::new)
        .collect::<Vec<_>>();
    map_chunks(pieces.len(), |chunk| {
        let mut piece = pieces[chunk].lock().unwrap_or_else(PoisonError::into_inner);
        fill(chunk, &mut piece);
    });
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn chunks_map_in_order_whatever_thread_takes_them() {
        for chunk_count in [0, 1, 2, 7, 1_000] {
            let squares = map_chunks(chunk_count, |chunk| chunk * chunk);

            assert_eq!(
                squares,
                (0..chunk_count)
                    .map(|chunk| chunk * chunk)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn nested_maps_finish_on_callers_and_on_pool_workers() {
        let sums = map_chunks(16, |outer| {
            map_chunks(16, |inner| outer * 16 + inner)
                .into_iter()
                .sum::<usize>()
        });

        assert_eq!(sums.iter().sum::<usize>(), (0..256).sum::<usize>());
        assert!(!CALLER_IS_DRAINING.get());
    }

    #[test]
    fn tree_maps_keep_helpers_and_the_outer_call_stays_marked() {
        let sums = map_nested_chunks(8, |outer| {
            let inner = map_nested_chunks(8, |inner| outer * 8 + inner);
            // Only a non-worker thread is marked.
            let marked = CALLER_IS_DRAINING.get();
            assert_eq!(marked, rayon::current_thread_index().is_none());
            inner.into_iter().sum::<usize>()
        });

        assert_eq!(sums.iter().sum::<usize>(), (0..64).sum::<usize>());
        assert!(!CALLER_IS_DRAINING.get());
    }

    #[test]
    fn a_pool_of_one_thread_maps_every_chunk_itself() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("a pool of one thread starts");

        let (doubled, threads) = pool.install(|| {
            let doubled = map_chunks(100, |chunk| (chunk * 2, rayon::current_thread_index()));
            (doubled, rayon::current_num_threads())
        });

        assert_eq!(threads, 1);
        assert!(
            doubled
                .iter()
                .enumerate()
                .all(|(chunk, &(double, thread))| double == chunk * 2 && thread == Some(0))
        );
    }

    #[test]
    fn a_panicking_chunk_fails_the_map_and_leaves_the_thread_usable() {
        const CHUNKS: usize = 5_000;
        let mapped = AtomicUsize::new(0);

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            map_chunks(CHUNKS, |chunk| {
                mapped.fetch_add(1, Ordering::Relaxed);
                assert_ne!(chunk, 0, "the first chunk fails");
                // Slow enough that the failure is seen long before the chunks run out.
                std::thread::sleep(Duration::from_millis(1));
                chunk
            })
        }));

        assert!(outcome.is_err());
        assert!(mapped.into_inner() < CHUNKS);
        assert!(!CALLER_IS_DRAINING.get());
        assert_eq!(map_chunks(3, |chunk| chunk + 1), [1, 2, 3]);
    }

    /// Triples chunk indexes on the pool and returns them as merged, which must happen on
    /// the calling thread.
    fn merged_triples(chunk_count: usize) -> Vec<(usize, usize)> {
        let caller = std::thread::current().id();
        let mut merged = Vec::with_capacity(chunk_count);
        merge_chunks(
            chunk_count,
            |chunk| {
                // Uneven work, so later chunks often finish before earlier ones.
                if chunk % 5 == 0 {
                    std::thread::yield_now();
                }
                chunk * 3
            },
            |chunk, tripled| {
                assert_eq!(std::thread::current().id(), caller);
                merged.push((chunk, tripled));
            },
        );
        merged
    }

    fn triples(chunk_count: usize) -> Vec<(usize, usize)> {
        (0..chunk_count).map(|chunk| (chunk, chunk * 3)).collect()
    }

    #[test]
    fn results_merge_in_order_on_the_calling_thread() {
        for chunk_count in [0, 1, 2, 7, 1_000] {
            let merged = merged_triples(chunk_count);

            assert_eq!(merged, triples(chunk_count));
        }
    }

    #[test]
    fn results_merge_in_order_on_pool_workers_and_in_nested_calls() {
        for threads in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("a small pool starts");

            let (merged, nested) = pool.install(|| {
                let mut nested = Vec::new();
                merge_chunks(
                    6,
                    |outer| {
                        let mut inner = Vec::new();
                        merge_chunks(6, |chunk| outer * 6 + chunk, |_, value| inner.push(value));
                        inner
                    },
                    |_, mut inner| nested.append(&mut inner),
                );
                (merged_triples(500), nested)
            });

            assert_eq!(merged, triples(500), "{threads} threads");
            assert_eq!(nested, (0..36).collect::<Vec<_>>(), "{threads} threads");
        }
    }

    #[test]
    fn a_panicking_merge_fails_the_call_and_stops_the_helpers() {
        const CHUNKS: usize = 5_000;
        let mapped = AtomicUsize::new(0);

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            merge_chunks(
                CHUNKS,
                |chunk| {
                    mapped.fetch_add(1, Ordering::Relaxed);
                    // Slow enough that the failure is seen long before the chunks run out.
                    std::thread::sleep(Duration::from_millis(1));
                    chunk
                },
                |chunk, _| assert_ne!(chunk, 3, "the fourth merge fails"),
            );
        }));

        assert!(outcome.is_err());
        assert!(mapped.into_inner() < CHUNKS);
        assert!(!CALLER_IS_DRAINING.get());
        assert_eq!(map_chunks(3, |chunk| chunk + 1), [1, 2, 3]);
    }

    #[test]
    fn an_adopted_chunk_holds_even_chunks_without_growing() {
        let mut merged = adopt_first_chunk(vec![1_u8; 100], 8, None);
        let reserved = merged.capacity();

        for _ in 1..8 {
            merged.append(&mut vec![1_u8; 100]);
        }

        assert_eq!(merged.len(), 800);
        assert_eq!(merged.capacity(), reserved);
    }

    #[test]
    fn an_adopted_chunk_reserves_no_more_than_its_limit() {
        let limited = adopt_first_chunk(vec![1_u8; 100], 64, Some(150));
        let full = adopt_first_chunk(vec![1_u8; 100], 64, Some(40));
        let empty = adopt_first_chunk(Vec::<u8>::new(), 0, None);

        assert_eq!(limited.capacity(), 150);
        assert_eq!((full.len(), full.capacity()), (100, 100));
        assert!(empty.is_empty());
    }

    #[test]
    fn chunks_join_in_order_up_to_a_limit_and_stop_being_mapped_past_it() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("a small pool starts");
        let every_element = (0..400).collect::<Vec<_>>();
        let mapped = AtomicUsize::new(0);
        let elements = |chunk: usize| {
            mapped.fetch_add(1, Ordering::Relaxed);
            // The first chunk is empty, so nothing is reserved from it.
            (chunk.saturating_sub(1) * 10..chunk * 10).collect::<Vec<_>>()
        };

        let limited = pool.install(|| {
            [None, Some(0), Some(1), Some(155), Some(400), Some(1_000)]
                .map(|most| concat_chunks(41, most, elements))
        });
        mapped.store(0, Ordering::Relaxed);
        let one_thread = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("a pool of one starts");
        let up_to_15 = one_thread.install(|| concat_chunks(41, Some(15), elements));
        let mapped_for_15 = mapped.swap(0, Ordering::Relaxed);
        let up_to_none = one_thread.install(|| concat_chunks(41, Some(0), elements));

        for (joined, most) in limited.iter().zip([400, 0, 1, 155, 400, 400]) {
            assert_eq!(joined[..], every_element[..most]);
        }
        assert_eq!(up_to_15[..], every_element[..15]);
        assert_eq!(mapped_for_15, 3);
        assert!(up_to_none.is_empty());
        assert_eq!(mapped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn room_past_the_largest_buffer_is_not_reserved() {
        let mut merged = vec![1_u64; 100];

        reserve_for_other_chunks(&mut merged, usize::MAX / 2, None);

        assert_eq!(merged, vec![1_u64; 100]);
        assert_eq!(merged.capacity(), 100);
    }

    #[test]
    fn mostly_unused_capacity_is_released() {
        let mut sparse = adopt_first_chunk(vec![1_u8; 100], 64, None);
        let mut dense = Vec::with_capacity(150);
        dense.extend([1_u8; 100]);

        release_unused_capacity(&mut sparse);
        release_unused_capacity(&mut dense);

        assert!(sparse.capacity() < 200);
        assert_eq!(dense.capacity(), 150);
    }

    #[test]
    fn pieces_fill_every_element_once() {
        for len in [0, 1, 63, 64, 65, 1_000] {
            let mut output = vec![0_usize; len];

            fill_chunks(&mut output, 64, |chunk, piece| {
                for (offset, element) in piece.iter_mut().enumerate() {
                    *element += chunk * 64 + offset + 1;
                }
            });

            assert_eq!(output, (1..=len).collect::<Vec<_>>());
        }
    }
}
