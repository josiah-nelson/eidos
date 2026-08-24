//! The seams sync nodes are written against.
//!
//! [`Env`] bundles the clock, transport, and timer surface a node sees
//! during a callback; [`Fs`] is durable state with explicit fsync points.
//! The simulation implements both in-process; production adapters implement
//! them over real sockets, timers, and files. A node must never reach around
//! these traits — that rule is what makes every protocol decision testable
//! under seeded fault injection.

use std::collections::BTreeMap;

/// Node index within one simulation (or one fleet wiring).
pub type NodeId = usize;

/// Simulated nanoseconds since the start of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SimTime(pub u64);

impl SimTime {
    pub fn plus(self, ns: u64) -> SimTime {
        SimTime(self.0 + ns)
    }
}

/// Clock seam used by protocol state machines.
pub trait Clock {
    fn now(&self) -> SimTime;
}

/// Fire-and-forget transport seam. Delivery is deliberately not promised.
pub trait Transport<M> {
    /// Fire-and-forget message send. The transport may drop, duplicate,
    /// delay, or reorder; nodes own all retry/idempotence logic.
    fn send(&mut self, to: NodeId, msg: M);
}

/// Volatile timer seam. Armed timers disappear with process RAM.
pub trait Timers {
    /// Arm a timer that calls `on_timer(timer)` after `after_ns`. Timers are
    /// lost on crash (they are RAM, not durable state).
    fn set_timer(&mut self, after_ns: u64, timer: u32);
}

/// Complete environment visible during one node callback.
pub trait Env<M>: Clock + Transport<M> + Timers {
    /// This node's durable storage.
    fn fs(&mut self) -> &mut dyn Fs;
}

/// Durable key-value state with explicit durability points.
///
/// `write` buffers; only `fsync` makes a value crash-survivable. On a
/// simulated crash every key reverts to its last fsynced value — exactly the
/// discipline the real applier needs for its same-transaction watermark.
pub trait Fs {
    fn write(&mut self, key: &str, value: Vec<u8>);
    fn fsync(&mut self, key: &str);
    fn read(&self, key: &str) -> Option<&[u8]>;
    /// Atomically install and durably commit several values. After a crash,
    /// either every write in the batch is visible or none is. The central
    /// applier uses this for the replicated effect and its watermark.
    fn write_atomic(&mut self, writes: Vec<(String, Vec<u8>)>);
    /// Convenience: write + fsync in one durability point.
    fn write_durable(&mut self, key: &str, value: Vec<u8>) {
        self.write(key, value);
        self.fsync(key);
    }
}

/// Simulated [`Fs`]: buffered vs. durable images per key.
#[derive(Debug, Default, Clone)]
pub struct SimFs {
    buffered: BTreeMap<String, Vec<u8>>,
    durable: BTreeMap<String, Vec<u8>>,
}

impl SimFs {
    /// Apply crash semantics: un-fsynced writes vanish.
    pub fn crash(&mut self) {
        self.buffered = self.durable.clone();
    }

    /// Durable image of a key, for invariant checks that must only trust
    /// what would survive a crash.
    pub fn durable(&self, key: &str) -> Option<&[u8]> {
        self.durable.get(key).map(|v| v.as_slice())
    }
}

impl Fs for SimFs {
    fn write(&mut self, key: &str, value: Vec<u8>) {
        self.buffered.insert(key.to_string(), value);
    }

    fn fsync(&mut self, key: &str) {
        if let Some(v) = self.buffered.get(key) {
            self.durable.insert(key.to_string(), v.clone());
        }
    }

    fn read(&self, key: &str) -> Option<&[u8]> {
        self.buffered.get(key).map(|v| v.as_slice())
    }

    fn write_atomic(&mut self, writes: Vec<(String, Vec<u8>)>) {
        for (key, value) in &writes {
            self.buffered.insert(key.clone(), value.clone());
        }
        for (key, value) in writes {
            self.durable.insert(key, value);
        }
    }
}

/// A sync state machine. Implementations hold only RAM state in `self`;
/// anything that must survive a crash goes through [`Env::fs`]. On restart
/// the simulation constructs a fresh instance (RAM gone) and calls
/// [`Node::on_start`] so the node recovers from durable state alone.
pub trait Node {
    type Msg: Clone + std::fmt::Debug;
    /// Called once at simulation start and again after every restart.
    fn on_start(&mut self, env: &mut dyn Env<Self::Msg>);
    fn on_message(&mut self, env: &mut dyn Env<Self::Msg>, from: NodeId, msg: Self::Msg);
    fn on_timer(&mut self, env: &mut dyn Env<Self::Msg>, timer: u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simfs_crash_discards_unsynced_writes() {
        let mut fs = SimFs::default();
        fs.write("a", b"one".to_vec());
        fs.fsync("a");
        fs.write("a", b"two".to_vec());
        fs.write("b", b"never-synced".to_vec());
        assert_eq!(fs.read("a"), Some(&b"two"[..]));
        fs.crash();
        assert_eq!(fs.read("a"), Some(&b"one"[..]));
        assert_eq!(fs.read("b"), None);
        assert_eq!(fs.durable("a"), Some(&b"one"[..]));
    }

    #[test]
    fn simfs_atomic_write_commits_every_key() {
        let mut fs = SimFs::default();
        fs.write_atomic(vec![
            ("effect".into(), b"row".to_vec()),
            ("watermark".into(), b"7".to_vec()),
        ]);
        fs.write("effect", b"torn".to_vec());
        fs.crash();
        assert_eq!(fs.read("effect"), Some(&b"row"[..]));
        assert_eq!(fs.read("watermark"), Some(&b"7"[..]));
    }
}
