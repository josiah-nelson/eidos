//! Protocol-level measurements the product emits because the observatory
//! cannot infer causality from system I/O alone (sprint section 7).

use crate::wire::Family;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use ts_rs::TS;

macro_rules! counters {
    ($name:ident, $view:ident { $($field:ident),* $(,)? }) => {
        #[derive(Debug, Default)]
        pub struct $name {
            $(pub $field: AtomicU64,)*
        }

        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
        pub struct $view {
            $(
                #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
                pub $field: u64,
            )*
        }

        impl $name {
            pub fn view(&self) -> $view {
                $view {
                    $($field: self.$field.load(Ordering::Relaxed),)*
                }
            }
        }
    };
}

counters!(
    FleetCounters,
    FleetCountersView {
        connections_attempted,
        connections_established_outbound,
        connections_established_inbound,
        connections_refused_unknown_peer,
        connections_refused_version,
        duplicate_sessions_closed,
        disconnects,
        enrollments,
        offers_sent,
        offers_received,
        batches_sent,
        batches_applied,
        rows_shipped,
        rows_applied,
        acks_sent,
        acks_received,
        duplicates_acknowledged,
        stale_batches,
        rejections_received,
        rejections_sent,
        fences,
        full_resyncs,
        repairs_offered,
        repairs_applied,
        repair_rows_applied,
        frames_refused_oversize,
        frames_malformed,
        bytes_control_sent,
        bytes_control_received,
        bytes_catalog_sent,
        bytes_catalog_received,
        bytes_repair_sent,
        bytes_repair_received,
        materialize_ms_total,
        apply_ms_total,
        backfill_steps,
        collections,
        tombstones_collected,
    }
);

impl FleetCounters {
    pub fn add(&self, counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn bytes_sent(&self, family: Family, n: u64) {
        match family {
            Family::Control => self.bytes_control_sent.fetch_add(n, Ordering::Relaxed),
            Family::Catalog => self.bytes_catalog_sent.fetch_add(n, Ordering::Relaxed),
            Family::Repair => self.bytes_repair_sent.fetch_add(n, Ordering::Relaxed),
        };
    }

    pub fn bytes_received(&self, family: Family, n: u64) {
        match family {
            Family::Control => self.bytes_control_received.fetch_add(n, Ordering::Relaxed),
            Family::Catalog => self.bytes_catalog_received.fetch_add(n, Ordering::Relaxed),
            Family::Repair => self.bytes_repair_received.fetch_add(n, Ordering::Relaxed),
        };
    }
}
