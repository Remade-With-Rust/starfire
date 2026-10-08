// SPDX-License-Identifier: Apache-2.0
//! `starfire-alloc` -- the one place Starfire decides how memory is allocated.
//!
//! Rust allows exactly one `#[global_allocator]` per program, so the
//! declaration lives in each deliverable's `main.rs`; everything else lives
//! here: which allocator at which version ([`Alloc`], required once in the
//! workspace manifest), how it is configured at startup ([`configure`]), and
//! whether hardening is on (the `secure` feature).
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: starfire_alloc::Alloc = starfire_alloc::Alloc;
//!
//! fn main() {
//!     starfire_alloc::configure(starfire_alloc::Profile::LongLived);
//! }
//! ```
//!
//! Libraries never declare the allocator and never depend on this crate: a
//! `#[global_allocator]` in a library is forced on every program that links it.

#![forbid(unsafe_code)]

/// The global allocator every Starfire deliverable installs.
pub use rusty_alloc_api::RustyAlloc as Alloc;

/// How long the process is expected to live: the only axis that changes the
/// runtime configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Apps and daemons. Enables purging, so freed spans go back to the OS.
    LongLived,
    /// One-shot tools. Leaves the allocator's defaults alone.
    ShortLived,
}

/// The option that gates returning free spans to the OS, resolved by NAME:
/// the index is upstream's internal ordering and its neighbours are options a
/// wrong index would set silently.
const PURGE_DELAY: &str = "purge_delay";

/// Purge immediately (upstream ships purging off).
const PURGE_IMMEDIATE: i64 = 0;

fn purge_delay_index() -> Option<usize> {
    rusty_alloc::options::OPTION_NAMES
        .iter()
        .position(|n| *n == PURGE_DELAY)
}

/// What [`configure`] did, so a deliverable can log it at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The profile requested.
    pub profile: Profile,
    /// `purge_delay` now in effect (`-1` = purging off).
    pub purge_delay: i64,
    /// Whether this build has the hardening feature compiled in. Cargo
    /// features are additive across a build, so read this off the running
    /// process rather than reasoning from manifests.
    pub secure: bool,
}

/// Configure the process-wide allocator. Call once, early in `main`, after
/// installing [`Alloc`]. Idempotent and never panics: an option that cannot
/// be resolved leaves the allocator at its defaults and shows in the result.
pub fn configure(profile: Profile) -> Applied {
    let idx = purge_delay_index();
    if profile == Profile::LongLived {
        if let Some(i) = idx {
            rusty_alloc::options::set(i, PURGE_IMMEDIATE);
        }
    }
    Applied {
        profile,
        purge_delay: idx.map(rusty_alloc::options::get).unwrap_or(-1),
        secure: cfg!(feature = "secure"),
    }
}

/// The allocator version actually linked in, for startup logs.
pub fn version() -> &'static str {
    rusty_alloc_api::VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_delay_resolves_by_name() {
        let idx = purge_delay_index();
        assert!(idx.is_some(), "`purge_delay` must exist in OPTION_NAMES");
    }

    #[test]
    fn long_lived_enables_purging_and_is_idempotent() {
        let a = configure(Profile::LongLived);
        assert_eq!(a.purge_delay, PURGE_IMMEDIATE);
        assert_eq!(a, configure(Profile::LongLived));
        assert_eq!(a.secure, cfg!(feature = "secure"));
    }

    /// 0.3.2 and earlier carry use-after-frees on every target; this crate
    /// requires 2.2.5. Fails here if a resolver change ever drags it back.
    #[test]
    fn linked_allocator_is_at_least_2_2_5() {
        let parts: Vec<u32> = version()
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect();
        assert!(parts.len() >= 3, "unparseable version {:?}", version());
        assert!(
            (parts[0], parts[1], parts[2]) >= (2, 2, 5),
            "rusty_alloc {} is older than the required 2.2.5",
            version()
        );
    }
}
