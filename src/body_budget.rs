//! A process-wide ceiling on request-body bytes buffered inside Hamster.
//!
//! Per-request limits bound one body. They do not bound how many bodies exist
//! at once, and the measured aggregate is `max_connections * MAX_REQUEST_BODY`
//! plus overhead, which is roughly 1 GiB at the shipped defaults. Lowering
//! `max_connections` bounds that, but it also refuses cheap requests that cost
//! no body memory at all. This type decouples the two: connection admission
//! stays generous, buffered body bytes get their own ceiling.
//!
//! The design is deliberately the smallest thing that can hold a stated
//! invariant:
//!
//! - a reservation is taken *before* the memory it covers is allocated, so the
//!   counter is never behind reality;
//! - a reservation is an owned RAII value, so every success, error, timeout,
//!   rejection and dropped-future path releases it;
//! - an over-budget request is refused immediately. Nothing waits on a
//!   reservation, so this adds no queue, no fairness policy and no new way for
//!   one request to block behind another request's client.
//!
//! What this does not cover is stated where it is measured: response bodies,
//! the protocol frame a worker builds while the job is in flight, and
//! allocator fragmentation are all outside the counter.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

/// How often a budget may report that it is refusing requests.
///
/// A refusal is peer-controlled, so reporting each one would let hostile
/// traffic turn the refusal path into a synchronous logging workload -- the
/// same failure the connection path already avoids. One line per window
/// reports the condition at a rate the peer cannot influence.
const REFUSAL_NOTICE_PERIOD_MS: u64 = 60_000;

/// Bytes of request body Hamster may have buffered at one time.
///
/// A `limit` of zero disables tracking entirely. That is what restores the
/// exact pre-budget code path, and it is what the hot-path A/B compares
/// against.
#[derive(Debug)]
pub struct BodyBudget {
    limit: usize,
    reserved: AtomicUsize,
    peak: AtomicUsize,
    rejections: AtomicU64,
    /// Milliseconds since `started` at which the next refusal may be reported.
    next_notice_ms: AtomicU64,
    started: Instant,
}

impl BodyBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            reserved: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            rejections: AtomicU64::new(0),
            next_notice_ms: AtomicU64::new(0),
            started: Instant::now(),
        })
    }

    pub fn disabled() -> Arc<Self> {
        Self::new(0)
    }

    pub const fn is_enabled(&self) -> bool {
        self.limit != 0
    }

    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes currently reserved. Observability and tests only; a caller that
    /// branched on this would be racing.
    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Acquire)
    }

    /// High-water mark of `reserved` since startup.
    ///
    /// This is what makes the ceiling checkable in a test instead of only
    /// arguable. It is deliberately not reported at runtime: its only
    /// operational use is proactive sizing, which needs periodic sampling, and
    /// that is a metrics subsystem rather than an explanation for a refusal.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    /// Requests refused because granting them would have crossed the limit.
    pub fn rejections(&self) -> u64 {
        self.rejections.load(Ordering::Acquire)
    }

    /// Takes a reservation, or refuses. Never waits.
    ///
    /// A disabled budget hands back an untracked reservation, which costs one
    /// predictable branch and no atomic traffic.
    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Option<BodyReservation> {
        if !self.is_enabled() {
            return Some(BodyReservation::none());
        }
        if !self.acquire(bytes) {
            return None;
        }
        Some(BodyReservation {
            budget: Some(Arc::clone(self)),
            held: bytes,
        })
    }

    fn acquire(&self, extra: usize) -> bool {
        if extra == 0 {
            return true;
        }
        let mut current = self.reserved.load(Ordering::Relaxed);
        loop {
            let next = match current.checked_add(extra) {
                Some(next) if next <= self.limit => next,
                _ => {
                    self.report_refusal();
                    return false;
                }
            };
            match self.reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.note_peak(next);
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Counts a refusal and reports it if this thread wins the current window.
    ///
    /// Only reached when the budget is already refusing, so the clock read and
    /// the compare-exchange are off the served path entirely.
    fn report_refusal(&self) {
        let refused = self.rejections.fetch_add(1, Ordering::Relaxed) + 1;
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.claim_refusal_notice(elapsed_ms) {
            return;
        }
        errln!(
            "body memory budget exhausted: {refused} request(s) refused with 503 since start (server.max_body_memory = {} bytes); reported at most once per {} s",
            self.limit,
            REFUSAL_NOTICE_PERIOD_MS / 1_000
        );
    }

    /// True for exactly one caller per notice window.
    ///
    /// Separated from the reporting so the rate limit can be tested against an
    /// injected clock rather than by capturing standard error.
    fn claim_refusal_notice(&self, now_ms: u64) -> bool {
        let due = self.next_notice_ms.load(Ordering::Relaxed);
        // `u64::MAX` is the saturated deadline and means "never again". Without
        // the sentinel a clock at the end of its range would compare equal to
        // its own next deadline and report every refusal.
        if due == u64::MAX || now_ms < due {
            return false;
        }
        // Losing this race means another thread is reporting this window.
        self.next_notice_ms
            .compare_exchange(
                due,
                now_ms.saturating_add(REFUSAL_NOTICE_PERIOD_MS),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let previous = self.reserved.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(
            previous >= bytes,
            "body budget released more than it reserved"
        );
    }

    /// A relaxed read first, so the common case where the peak is unchanged
    /// costs a load rather than a read-modify-write.
    fn note_peak(&self, value: usize) {
        if value > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(value, Ordering::AcqRel);
        }
    }
}

/// Bytes a single request holds against the budget.
///
/// Owned by whoever owns the bytes. It starts in the body reader, moves into
/// the worker job with the reconstructed request, and is dropped when that job
/// is dropped. There is no path that frees the bytes without dropping this.
#[derive(Debug)]
pub(crate) struct BodyReservation {
    budget: Option<Arc<BodyBudget>>,
    held: usize,
}

impl BodyReservation {
    /// A reservation against no budget, for call sites that hold no body.
    pub(crate) const fn none() -> Self {
        Self {
            budget: None,
            held: 0,
        }
    }

    pub(crate) const fn held(&self) -> usize {
        self.held
    }

    /// Raises this reservation to `target` bytes, or refuses.
    ///
    /// Only the difference is acquired, so growing a body never releases what
    /// it already holds and then races to get it back.
    pub(crate) fn grow_to(&mut self, target: usize) -> bool {
        if target <= self.held {
            return true;
        }
        let Some(budget) = &self.budget else {
            self.held = target;
            return true;
        };
        if !budget.acquire(target - self.held) {
            return false;
        }
        self.held = target;
        true
    }
}

impl Drop for BodyReservation {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.release(self.held);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_budget_tracks_nothing_and_refuses_nothing() {
        let budget = BodyBudget::disabled();
        let mut reservation = budget.reserve(usize::MAX).expect("never refuses");
        assert!(reservation.grow_to(usize::MAX));
        assert_eq!(budget.reserved(), 0);
        assert_eq!(budget.peak(), 0);
        assert_eq!(budget.rejections(), 0);
    }

    #[test]
    fn reservations_are_released_when_dropped() {
        let budget = BodyBudget::new(1_000);
        {
            let _first = budget.reserve(400).expect("fits");
            let _second = budget.reserve(400).expect("fits");
            assert_eq!(budget.reserved(), 800);
        }
        assert_eq!(budget.reserved(), 0);
        assert_eq!(budget.peak(), 800);
    }

    #[test]
    fn a_reservation_past_the_limit_is_refused_without_disturbing_the_counter() {
        let budget = BodyBudget::new(1_000);
        let held = budget.reserve(900).expect("fits");
        assert!(budget.reserve(200).is_none());
        assert_eq!(budget.reserved(), 900, "a refusal must not consume budget");
        assert_eq!(budget.rejections(), 1);
        drop(held);
        assert_eq!(budget.reserved(), 0);
        assert!(budget.reserve(1_000).is_some(), "the limit itself fits");
    }

    #[test]
    fn growth_acquires_only_the_difference_and_a_refused_growth_keeps_the_original() {
        let budget = BodyBudget::new(1_000);
        let mut reservation = budget.reserve(100).expect("fits");
        assert!(reservation.grow_to(600));
        assert_eq!(budget.reserved(), 600);
        assert_eq!(reservation.held(), 600);
        assert!(!reservation.grow_to(1_400), "past the limit");
        assert_eq!(
            budget.reserved(),
            600,
            "a refused growth must leave the reservation intact"
        );
        assert_eq!(reservation.held(), 600);
        assert!(
            reservation.grow_to(300),
            "shrinking is a no-op, not a release"
        );
        assert_eq!(budget.reserved(), 600);
    }

    #[test]
    fn every_refusal_is_counted_but_only_one_per_window_is_reported() {
        let counting = BodyBudget::new(100);
        let _held = counting.reserve(100).expect("fills the budget");

        // The rate limit is on the reporting, never on the counting: an
        // operator reading the count must not see a number that was silently
        // reduced to keep standard error quiet. This also emits exactly one
        // real notice, which is the mechanism end to end.
        for _ in 0..50 {
            assert!(counting.reserve(1).is_none());
        }
        assert_eq!(counting.rejections(), 50);

        // A budget whose window has not been consumed yet, so the clock
        // arithmetic can be driven directly.
        let budget = BodyBudget::new(100);
        // The first refusal of a run reports immediately, so the operator
        // learns about the condition when it starts rather than a window later.
        assert!(budget.claim_refusal_notice(0));
        assert!(!budget.claim_refusal_notice(1));
        assert!(!budget.claim_refusal_notice(REFUSAL_NOTICE_PERIOD_MS - 1));
        assert!(budget.claim_refusal_notice(REFUSAL_NOTICE_PERIOD_MS));
        assert!(!budget.claim_refusal_notice(REFUSAL_NOTICE_PERIOD_MS + 1));

        // A clock at the end of its range must not wrap the next deadline
        // backwards and turn the rate limit off.
        assert!(budget.claim_refusal_notice(u64::MAX));
        assert!(!budget.claim_refusal_notice(u64::MAX));
    }

    #[test]
    fn concurrent_refusals_report_at_most_once_per_window() {
        use std::{sync::Barrier, thread};

        const THREADS: usize = 16;

        let budget = BodyBudget::new(100);
        let _held = budget.reserve(100).expect("fills the budget");
        let barrier = Arc::new(Barrier::new(THREADS));
        let reporters = Arc::new(AtomicU64::new(0));

        thread::scope(|scope| {
            for _ in 0..THREADS {
                let budget = Arc::clone(&budget);
                let barrier = Arc::clone(&barrier);
                let reporters = Arc::clone(&reporters);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..200 {
                        if budget.claim_refusal_notice(0) {
                            reporters.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert_eq!(
            reporters.load(Ordering::Relaxed),
            1,
            "a burst of refusals must produce one notice, not one per thread"
        );
    }

    #[test]
    fn a_disabled_budget_never_counts_or_reports_a_refusal() {
        let budget = BodyBudget::disabled();
        for _ in 0..10 {
            assert!(budget.reserve(usize::MAX).is_some());
        }
        assert_eq!(budget.rejections(), 0);
    }

    #[test]
    fn overflow_is_refused_rather_than_wrapping() {
        let budget = BodyBudget::new(usize::MAX);
        let _held = budget.reserve(usize::MAX).expect("fits exactly");
        assert!(budget.reserve(1).is_none());
        assert_eq!(budget.reserved(), usize::MAX);
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        use std::{sync::Barrier, thread};

        const THREADS: usize = 16;
        const ROUNDS: usize = 500;
        const CHUNK: usize = 64;
        const LIMIT: usize = CHUNK * 8;

        let budget = BodyBudget::new(LIMIT);
        let barrier = Arc::new(Barrier::new(THREADS));
        let granted = Arc::new(AtomicU64::new(0));

        thread::scope(|scope| {
            for _ in 0..THREADS {
                let budget = Arc::clone(&budget);
                let barrier = Arc::clone(&barrier);
                let granted = Arc::clone(&granted);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        if let Some(reservation) = budget.reserve(CHUNK) {
                            granted.fetch_add(1, Ordering::Relaxed);
                            assert!(
                                budget.reserved() <= LIMIT,
                                "the counter crossed the limit under contention"
                            );
                            drop(reservation);
                        }
                    }
                });
            }
        });

        assert_eq!(budget.reserved(), 0, "every reservation was released");
        assert!(budget.peak() <= LIMIT, "peak {} > limit", budget.peak());
        assert!(granted.load(Ordering::Relaxed) > 0, "nothing was granted");
    }
}
