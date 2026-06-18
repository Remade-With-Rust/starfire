// SPDX-License-Identifier: Apache-2.0
//! Starfire desktop consumer.
//!
//! Phase 0: a placeholder that constructs a session and reports the scaffold is
//! wired. The Dioxus UI, render surface, and input capture land in Phase 1
//! (docs/05-build-plan.md). Kept deliberately dependency-light until then.

use starfire_core::session::Session;

fn main() {
    let session = Session::new();
    println!("Starfire scaffold — phase {:?}.", session.phase());
    println!("Protocol core, platform trait seams, and test harness are wired.");
    println!("See docs/05-build-plan.md for what lands next.");
}
