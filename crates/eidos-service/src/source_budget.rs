//! Per-source content concurrency budgets and atomic reservations.
//!
//! A source's `content_concurrency` bounds how many workers may read from
//! that volume at once, so a slow HDD or an SMB share cannot be hammered by
//! the whole pool. Capacity is taken through [`SourceBudgets::try_reserve`]
//! **before** any job is claimed: the check and the increment happen under
//! one mutex, so two workers can never both see the same free slot.
//!
//! The returned [`SourceReservation`] is an RAII guard. It releases its unit
//! when dropped, which covers the normal path, an empty or failed claim, a
//! pipeline error, cancellation, shutdown, and unwinding from a panic.
//!
//! Lock ordering: this mutex is a leaf. Reservations are taken while the
//! catalog writer is held (inside the claiming transaction); nothing may
//! take a catalog lock while holding this one.

use eidos_domain::SourceId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Budget assumed for a source whose policy has not been loaded yet
/// (matches the `sources.content_concurrency` column default).
pub const DEFAULT_BUDGET: u32 = 2;

#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    /// `None` until the coordinator has read this source's policy.
    budget: Option<u32>,
    reserved: u32,
    /// High-water mark of `reserved` since process start.
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
}

impl SourceReservation {
    pub fn source(&self) -> SourceId {
        self.source
    }
}

impl Drop for SourceReservation {
    fn drop(&mut self) {
        self.budgets.release(self.source);
    }
}

/// Diagnostic view of one source's budget and live reservations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceConcurrencyView {
    pub source_id: SourceId,
    pub budget: u32,
    pub reserved: u32,
    pub peak_reserved: u32,
}

impl SourceBudgets {
    /// Replace the known budgets (the coordinator refreshes these from
    /// source policy every enqueue interval). Live reservations survive;
    /// a source missing from `budgets` falls back to [`DEFAULT_BUDGET`].
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

    /// Take one unit of `id`'s budget, or `None` when it is exhausted.
    /// A budget of zero never admits work.
    pub fn try_reserve(self: &Arc<Self>, id: SourceId) -> Option<SourceReservation> {
        {
            let mut slots = self.slots.lock();
            let slot = slots.entry(id).or_default();
            if slot.reserved >= slot.budget() {
                return None;
            }
            slot.reserved += 1;
            slot.peak = slot.peak.max(slot.reserved);
        }
        Some(SourceReservation {
            budgets: self.clone(),
            source: id,
        })
    }

    fn release(&self, id: SourceId) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(&id) {
            slot.reserved = slot.reserved.saturating_sub(1);
        }
    }

    /// Sources with no free capacity right now.
    pub fn saturated(&self) -> Vec<SourceId> {
        self.slots
            .lock()
            .iter()
            .filter(|(_, s)| s.reserved >= s.budget())
            .map(|(id, _)| *id)
            .collect()
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
        let r1 = b.try_reserve(A).expect("first unit");
        let r2 = b.try_reserve(A).expect("second unit");
        assert!(b.try_reserve(A).is_none(), "budget of two is exhausted");
        assert_eq!(b.reserved(A), 2);
        assert_eq!(b.saturated(), vec![A]);
        drop(r1);
        assert_eq!(b.reserved(A), 1);
        assert!(b.saturated().is_empty());
        assert!(b.try_reserve(A).is_some());
        drop(r2);
        assert_eq!(b.peak_reserved(A), 2, "high-water mark is kept");
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
    fn unknown_sources_use_the_default_budget_and_refresh_keeps_reservations() {
        let b = Arc::new(SourceBudgets::default());
        assert_eq!(b.budget(A), DEFAULT_BUDGET);
        let held = b.try_reserve(A).expect("unit");
        b.set_all(&HashMap::from([(A, 1), (SourceId(2), 4)]));
        assert_eq!(b.budget(A), 1);
        assert_eq!(b.reserved(A), 1, "refresh must not lose live reservations");
        assert!(b.try_reserve(A).is_none(), "now over the lowered budget");
        drop(held);
        assert!(b.try_reserve(A).is_some());
        // A source dropped from the refresh falls back to the default.
        b.set_all(&HashMap::new());
        assert_eq!(b.budget(SourceId(2)), DEFAULT_BUDGET);
    }

    #[test]
    fn a_zero_budget_admits_nothing() {
        let b = Arc::new(SourceBudgets::default());
        b.set(A, 0);
        assert!(b.try_reserve(A).is_none());
        assert_eq!(b.saturated(), vec![A]);
    }
}
