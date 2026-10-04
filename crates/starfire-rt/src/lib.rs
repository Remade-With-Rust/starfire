// SPDX-License-Identifier: Apache-2.0
//! Real-time scheduling hints for the streaming threads.
//!
//! A streaming pipeline is a chain of short hand-offs: packet in, frame out. Each
//! thread in it does very little work, but it has to run **now** — if the OS
//! leaves it runnable-but-waiting behind a busy machine's other work, the frame
//! is late by however long that wait was.
//!
//! What it buys, measured on the host with 28 busy-loop processes running
//! beside the stream (off / on interleaved, same build): desktop image presented
//! -> frame sent fell from 19.6-22.6 ms to 16.9-18.6 ms at the median, the
//! capture+encode stage stopped stretching (15.3-19.5 ms -> 14.7-15.1 ms), and
//! the stream held 52-58 fps instead of 46-55. The 99th percentile was lower in
//! most windows but not cleanly. On a machine with idle cores it changes
//! nothing measurable, and the client showed no clear effect in that test.
//!
//! [`promote_current_thread`] asks the OS to schedule the calling thread ahead
//! of ordinary work. It is a hint and never fails the caller: on a platform or
//! account where it is not allowed, the thread simply stays as it was.
//!
//! # Safety
//! This crate is the single home of the OS calls involved, so the crates that
//! use it need no `unsafe`. Each call takes plain integers / a pseudo-handle for
//! the *current* thread and has no memory-safety preconditions.

use std::sync::atomic::{AtomicBool, Ordering};

/// What a thread does, which decides how hard to promote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Moves packets: blocks almost all the time and does microseconds of work
    /// when woken (network receive, paced send, input forwarding). Being
    /// scheduled immediately is the whole point, and it cannot starve anything.
    Network,
    /// Does bounded per-frame work (reassemble + decode, capture + encode).
    /// Runs ahead of ordinary work, below the network threads.
    Frame,
}

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turn promotion on or off process-wide (on by default). Off makes
/// [`promote_current_thread`] a no-op — the A/B switch for measuring what the
/// promotion buys on a given machine.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether promotion is enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Ask the OS to schedule the **calling** thread ahead of ordinary work, as
/// appropriate for `role`. Returns whether the OS accepted the request. Call it
/// once, first thing, on the thread itself.
pub fn promote_current_thread(role: Role) -> bool {
    if !enabled() {
        return false;
    }
    imp::promote(role)
}

#[cfg(windows)]
mod imp {
    use super::Role;

    // Win32 thread priority levels (relative to the process priority class).
    const THREAD_PRIORITY_HIGHEST: i32 = 2;
    const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    }

    pub fn promote(role: Role) -> bool {
        let level = match role {
            Role::Network => THREAD_PRIORITY_TIME_CRITICAL,
            Role::Frame => THREAD_PRIORITY_HIGHEST,
        };
        // SAFETY: `GetCurrentThread` returns a pseudo-handle that is always valid
        // for the calling thread and needs no closing; `SetThreadPriority` takes
        // that handle and an integer and touches no caller memory.
        unsafe { SetThreadPriority(GetCurrentThread(), level) != 0 }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Role;

    /// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`: the class the scheduler
    /// runs first and keeps on the performance cores.
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }

    pub fn promote(_role: Role) -> bool {
        // SAFETY: sets the QoS class of the calling thread only; takes two
        // integers and touches no caller memory.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) == 0 }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::Role;

    /// No portable unprivileged way to raise a thread above normal priority on
    /// other platforms; leave the thread as it is.
    pub fn promote(_role: Role) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Promotion is accepted for both roles where the platform supports it, is
    /// declined harmlessly elsewhere, and is a no-op when switched off (the A/B
    /// baseline). One test, because the switch is process-wide.
    #[test]
    fn promotion_is_accepted_or_harmlessly_declined() {
        let supported = cfg!(any(windows, target_os = "macos"));
        let outcome = std::thread::spawn(move || {
            let net = promote_current_thread(Role::Network);
            let frame = promote_current_thread(Role::Frame);
            set_enabled(false);
            let off = promote_current_thread(Role::Network);
            set_enabled(true);
            (net, frame, off)
        })
        .join();
        let Ok((net, frame, off)) = outcome else {
            panic!("promoting a thread must not panic it");
        };
        assert_eq!((net, frame), (supported, supported));
        assert!(!off, "disabled means no-op");
        assert!(enabled());
    }
}
