//! Per-source content concurrency budgets and atomic reservations.
//!
//! A source's `content_concurrency` bounds how many workers may read from
//! that source at once. This is not a shared-device cap: multiple roots or
//! partitions on the same disk can still contend. Capacity is taken through [`SourceBudgets::try_reserve`]
//! **before** any job is claimed: the check and the increment happen under
//! one mutex, so two workers can never both see the same free slot.
//!
//! The returned [`SourceReservation`] is an RAII guard. It releases its unit
//! when dropped, which covers the normal path, an empty or failed claim, a
//! pipeline error, cancellation, shutdown, and unwinding from a panic.
//!
//! A claim passes more than one gate. [`SourceReservation::confirm`] marks the
//! unit as kept once they all grant, so the reported high-water mark describes
//! work this source was admitted for rather than attempts another ceiling
//! refused. Admission itself is unaffected: an unconfirmed unit still occupies
//! the budget for as long as it is held.
//!
//! Lock ordering: this mutex is a leaf. Reservations are taken while the
//! catalog writer is held (inside the claiming transaction); nothing may
//! take a catalog lock while holding this one.

use eidos_domain::SourceId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use ts_rs::TS;

/// Budget *reported* for a source whose policy has not been read yet
/// (matches the `sources.content_concurrency` column default). It is a
/// display value only: an unknown source admits no work at all, so a
/// failed policy load can never oversubscribe a slow volume.
pub const DEFAULT_BUDGET: u32 = 2;

#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    /// `None` until the coordinator has read this source's policy.
    budget: Option<u32>,
    reserved: u32,
    /// Reservations whose caller went on to use them. A claim is admitted by
    /// more than this budget, so a unit taken here and released because a
    /// *different* gate refused the same claim is never work this source did.
    confirmed: u32,
    /// High-water mark of `confirmed` since process start.
    peak: u32,
}

impl Slot {
    fn budget(&self) -> u32 {
        self.budget.unwrap_or(DEFAULT_BUDGET)
    }
}

/// Per-source concurrency budgets plus the reservations currently held
/// against them.
#[derive(Debug, Default)]
pub struct SourceBudgets {
    slots: Mutex<HashMap<SourceId, Slot>>,
}

/// One unit of a source's concurrency budget, released on drop.
#[derive(Debug)]
pub struct SourceReservation {
    budgets: Arc<SourceBudgets>,
    source: SourceId,
    confirmed: bool,
}

impl SourceReservation {
    /// Count this unit towards the reported high-water mark. Call it once the
    /// claim's other admission gates have also granted, so the diagnostic
    /// describes reservations this source kept rather than attempts a shared
    /// ceiling refused. Releasing without confirming leaves the peak alone.
    pub fn confirm(&mut self) {
        if !self.confirmed {
            self.confirmed = true;
            let mut slots = self.budgets.slots.lock();
            let slot = slots.entry(self.source).or_default();
            slot.confirmed += 1;
            slot.peak = slot.peak.max(slot.confirmed);
        }
    }

    pub fn source(&self) -> SourceId {
        self.source
    }
}

impl Drop for SourceReservation {
    fn drop(&mut self) {
        self.budgets.release(self.source, self.confirmed);
    }
}

/// Diagnostic view of one source's budget and live reservations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, TS)]
pub struct SourceConcurrencyView {
    pub source_id: SourceId,
    pub budget: u32,
    pub reserved: u32,
    /// High-water mark of reservations this source actually kept. A unit
    /// released because another admission gate refused the same claim is
    /// excluded, so this does not climb while a shared ceiling holds the work.
    pub peak_reserved: u32,
}

impl SourceBudgets {
    /// Replace the known budgets (the coordinator refreshes these from
    /// source policy every enqueue interval, and startup does it once
    /// before the pool starts). Live reservations survive; a source missing
    /// from `budgets` becomes unknown again and admits nothing.
    pub fn set_all(&self, budgets: &HashMap<SourceId, u32>) {
        let mut slots = self.slots.lock();
        for (id, slot) in slots.iter_mut() {
            slot.budget = budgets.get(id).copied();
        }
        for (id, budget) in budgets {
            slots.entry(*id).or_default().budget = Some(*budget);
        }
    }

    /// Set one source's budget (an API policy change takes effect at once).
    /// Lowering it below the live reservation count does not revoke work
    /// already in flight; no further unit is handed out until it drains.
    pub fn set(&self, id: SourceId, budget: u32) {
        self.slots.lock().entry(id).or_default().budget = Some(budget);
    }

    pub fn budget(&self, id: SourceId) -> u32 {
        self.slots
            .lock()
            .get(&id)
            .map(Slot::budget)
            .unwrap_or(DEFAULT_BUDGET)
    }

    pub fn reserved(&self, id: SourceId) -> u32 {
        self.slots.lock().get(&id).map(|s| s.reserved).unwrap_or(0)
    }

    pub fn peak_reserved(&self, id: SourceId) -> u32 {
        self.slots.lock().get(&id).map(|s| s.peak).unwrap_or(0)
    }

    /// Units held across every source: one per batch being extracted, and so
    /// the count of workers that are mid-batch. Unlike [`Self::snapshot`]
    /// this allocates nothing, which is what lets `/api/health` report the
    /// pipeline's flow on every call.
    pub fn reserved_total(&self) -> u32 {
        self.slots.lock().values().map(|s| s.reserved).sum()
    }

    /// Take one unit of `id`'s budget, or `None` when it is exhausted.
    ///
    /// A source whose policy has not been read yet admits nothing, so a
    /// catalog read that fails at startup makes workers idle rather than
    /// fall back to a budget larger than the source is configured for. The
    /// next successful refresh unblocks them. A budget of zero likewise
    /// never admits work.
    pub fn try_reserve(self: &Arc<Self>, id: SourceId) -> Option<SourceReservation> {
        {
            let mut slots = self.slots.lock();
            let slot = slots.entry(id).or_default();
            if slot.budget.is_none() || slot.reserved >= slot.budget() {
                return None;
            }
            slot.reserved += 1;
        }
        Some(SourceReservation {
            budgets: self.clone(),
            source: id,
            confirmed: false,
        })
    }

    fn release(&self, id: SourceId, confirmed: bool) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(&id) {
            slot.reserved = slot.reserved.saturating_sub(1);
            if confirmed {
                slot.confirmed = slot.confirmed.saturating_sub(1);
            }
        }
    }

    /// Diagnostics for `GET /api/activity`, ordered by source id.
    pub fn snapshot(&self) -> Vec<SourceConcurrencyView> {
        let mut out: Vec<SourceConcurrencyView> = self
            .slots
            .lock()
            .iter()
            .map(|(id, s)| SourceConcurrencyView {
                source_id: *id,
                budget: s.budget(),
                reserved: s.reserved,
                peak_reserved: s.peak,
            })
            .collect();
        out.sort_by_key(|v| v.source_id.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: SourceId = SourceId(1);

    #[test]
    fn reservations_are_bounded_by_the_budget_and_released_on_drop() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 2);
        let mut r1 = b.try_reserve(A).expect("first unit");
        let mut r2 = b.try_reserve(A).expect("second unit");
        r1.confirm();
        r2.confirm();
        assert!(b.try_reserve(A).is_none(), "budget of two is exhausted");
        assert_eq!(b.reserved(A), 2);
        drop(r1);
        assert_eq!(b.reserved(A), 1);
        assert!(b.try_reserve(A).is_some());
        drop(r2);
        assert_eq!(b.peak_reserved(A), 2, "high-water mark is kept");
    }

    #[test]
    fn a_unit_released_because_another_gate_refused_stays_out_of_the_peak() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 2);
        // A claim takes a unit here and is then refused by the shared-device
        // ceiling. It never read anything, so it is not this source's peak.
        for _ in 0..8 {
            drop(b.try_reserve(A).expect("unit"));
        }
        assert_eq!(b.peak_reserved(A), 0);

        // A losing racer holding its unit alongside a winner is still excluded.
        let mut winner = b.try_reserve(A).expect("winner");
        winner.confirm();
        let loser = b.try_reserve(A).expect("loser");
        assert_eq!(b.reserved(A), 2, "both units are genuinely held");
        assert_eq!(b.peak_reserved(A), 1, "only the confirmed unit counts");
        drop(loser);

        // Confirming is idempotent and survives release of the other unit.
        winner.confirm();
        assert_eq!(b.peak_reserved(A), 1);
        drop(winner);
        assert_eq!(b.reserved(A), 0);
        let mut again = b.try_reserve(A).expect("unit");
        again.confirm();
        assert_eq!(
            b.peak_reserved(A),
            1,
            "the mark is a high-water, not a count"
        );
    }

    #[test]
    fn a_reservation_is_released_while_unwinding() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 1);
        let b2 = b.clone();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = b2.try_reserve(A).expect("unit");
            panic!("extraction blew up");
        }));
        assert!(out.is_err());
        assert_eq!(b.reserved(A), 0, "guard released during unwind");
        assert!(b.try_reserve(A).is_some());
    }

    #[test]
    fn a_source_admits_nothing_until_its_policy_is_known() {
        let b = Arc::new(SourceBudgets::default());
        assert_eq!(b.budget(A), DEFAULT_BUDGET, "reported for display");
        assert!(
            b.try_reserve(A).is_none(),
            "an unread policy must not fall back to the default budget"
        );
        b.set_all(&HashMap::from([(A, 1)]));
        assert!(b.try_reserve(A).is_some(), "unblocked by the refresh");
    }

    #[test]
    fn a_refresh_keeps_live_reservations() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 2);
        let held = b.try_reserve(A).expect("unit");
        b.set_all(&HashMap::from([(A, 1), (SourceId(2), 4)]));
        assert_eq!(b.budget(A), 1);
        assert_eq!(b.reserved(A), 1, "refresh must not lose live reservations");
        assert!(b.try_reserve(A).is_none(), "now over the lowered budget");
        drop(held);
        assert!(b.try_reserve(A).is_some());
        // A source dropped from the refresh is unknown again, not default.
        b.set_all(&HashMap::new());
        assert_eq!(b.budget(SourceId(2)), DEFAULT_BUDGET);
        assert!(b.try_reserve(SourceId(2)).is_none());
    }

    #[test]
    fn a_zero_budget_admits_nothing() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 0);
        assert!(b.try_reserve(A).is_none());
        assert_eq!(b.budget(A), 0);
    }
}
