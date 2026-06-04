//! Loom concurrency models for the lock-bearing and lock-free primitives that
//! the storage engine's correctness depends on (Task 185).
//!
//! Loom is an exhaustive model checker: it enumerates every legal interleaving
//! (and every legal memory-ordering outcome) of the threads spawned inside
//! [`loom::model`], so a property asserted here is checked against the whole
//! reachable state space, not just whatever schedule the OS happened to pick.
//!
//! These tests compile and run **only** under `--cfg rgraph_loom` (a
//! crate-private cfg name chosen so the flag does not leak into dependencies
//! like tokio, which have their own `loom` cfg handling):
//!
//! ```text
//! RUSTFLAGS="--cfg rgraph_loom" cargo test --test loom_concurrency --release
//! ```
//!
//! Without the flag the file is an empty crate (a single `main`-less, test-less
//! translation unit), so the regular `cargo test` graph never pays for loom and
//! `cargo test --test loom_concurrency` reports zero tests rather than failing.
//!
//! # Models
//!
//! 1. [`latch_exclusive_excludes_all`] — the real [`LatchCoupling`] (whose
//!    `Mutex`/`Condvar` are swapped for `loom::sync` equivalents under `--cfg
//!    rgraph_loom`) never lets a shared and an exclusive holder observe the page
//!    at the same time, and the writer's release bumps the optimistic-read
//!    version.
//! 2. [`pin_count_blocks_eviction`] — a faithful model of the buffer pool's
//!    shard-lock-serialised pin versus evict protocol from `src/buffer/pool.rs`:
//!    a frame that a thread has pinned under the shard map lock is never evicted
//!    out from under it by a concurrent evictor.

// The entire suite is loom-only.  Under a normal build the module body is empty
// so libtest finds no tests and the binary links cleanly.
#![cfg(rgraph_loom)]

use loom::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use loom::sync::{Arc, Mutex};
use rgraph::index::{LatchCoupling, LatchMode};

/// Model 1 — mutual exclusion of the page latch.
///
/// One writer takes an exclusive latch on a page and the other thread takes a
/// shared latch on the *same* page.  Loom explores every interleaving; in every
/// one the two critical sections must be serialised (the exclusive latch and the
/// shared latch can never be held simultaneously), and the page version must be
/// strictly greater after the exclusive writer has released than before it ran.
#[test]
fn latch_exclusive_excludes_all() {
    loom::model(|| {
        const PAGE: u64 = 1;

        let latch = Arc::new(LatchCoupling::new());
        // A flag, written only while the exclusive latch is held, that a shared
        // reader must never observe as `true` (which would prove overlap).
        let writer_inside = Arc::new(AtomicU8::new(0));

        let v_before = latch.version(PAGE);

        let w_latch = Arc::clone(&latch);
        let w_flag = Arc::clone(&writer_inside);
        let writer = loom::thread::spawn(move || {
            let guard = w_latch.latch(PAGE, LatchMode::Exclusive);
            // Inside the exclusive critical section.
            w_flag.store(1, Ordering::SeqCst);
            // ... mutation would happen here ...
            w_flag.store(0, Ordering::SeqCst);
            drop(guard); // release bumps the version
        });

        let r_latch = Arc::clone(&latch);
        let r_flag = Arc::clone(&writer_inside);
        let reader = loom::thread::spawn(move || {
            let guard = r_latch.latch(PAGE, LatchMode::Shared);
            // If we hold a shared latch, no exclusive writer may be inside its
            // critical section: mutual exclusion is the property under test.
            assert_eq!(
                r_flag.load(Ordering::SeqCst),
                0,
                "shared and exclusive latch holders overlapped"
            );
            drop(guard);
        });

        writer.join().unwrap();
        reader.join().unwrap();

        // The exclusive release must have advanced the optimistic-read version
        // exactly once; the shared release must not have touched it.
        let v_after = latch.version(PAGE);
        assert_eq!(
            v_after,
            v_before.wrapping_add(1),
            "exclusive release must bump the version exactly once"
        );
    });
}

/// Model 2 — a pinned buffer frame is never evicted out from under its pinner.
///
/// This is a faithful model of the shard-lock-serialised pin/evict protocol in
/// `BufferPool::fix_page` / `BufferPool::sweep_for_victim`.  The decisive
/// detail — which an atomics-only model gets wrong — is that the pin and the
/// eviction handshake **share the per-shard mutex**:
///
/// * The **pinner** (mirroring `fix_page`'s resident fast path) takes the shard
///   lock, looks the page up in the map, and only if it is still mapped bumps
///   `pin_count` with `fetch_add`, all *under the lock*.  A page the evictor has
///   already unmapped is simply a miss.
/// * The **evictor** (mirroring `sweep_for_victim`) takes the same shard lock,
///   re-checks `pin_count` **under the lock**, and only commits the eviction
///   (unmap + reset, modelled as `state = EVICTED`) when the pin count is zero.
///   A non-zero pin makes it abort and leave the frame mapped and Resident.
///
/// Because both the pin and the pin-recheck happen under the one mutex, they are
/// linearised: a pinner that observes the page mapped has its `fetch_add` ordered
/// before any evictor that subsequently takes the lock, so the evictor sees the
/// pin and backs off.  Loom verifies the invariant **a frame is never EVICTED
/// while a pin taken under the mapped state is outstanding** across every
/// interleaving and memory ordering.
///
/// > Note for the engine: the real `sweep_for_victim` performs its *first*
/// > `is_pinned()` check **before** taking the shard lock and relies on the
/// > under-lock map ownership recheck for correctness.  Modelling the recheck
/// > under the lock (as here) is the sound reference; if a future change moves
/// > the authoritative pin-check out from under the shard lock, this model is
/// > the specification it must continue to satisfy.
///
/// `mapped` (the shard map cell): `true` while the frame owns the page.
/// State byte encoding: 0 = Resident, 2 = Evicted.
#[test]
fn pin_count_blocks_eviction() {
    const RESIDENT: u8 = 0;
    const EVICTED: u8 = 2;

    loom::model(|| {
        // The shard map cell guards both the pin and the evict handshake.  We
        // model the map entry as a `bool` (`true` = page still mapped to this
        // frame) behind the shard mutex.
        let shard = Arc::new(Mutex::new(true));
        let pin_count = Arc::new(AtomicU16::new(0));
        let state = Arc::new(AtomicU8::new(RESIDENT));

        // Pinner: take the shard lock, and pin only if the page is still mapped.
        // Returns `true` iff it took a confirmed pin on a mapped frame.
        let p_shard = Arc::clone(&shard);
        let p_pin = Arc::clone(&pin_count);
        let p_state = Arc::clone(&state);
        let pinner = loom::thread::spawn(move || {
            let pinned = {
                let mapped = p_shard.lock().unwrap();
                if *mapped {
                    p_pin.fetch_add(1, Ordering::Relaxed);
                    true
                } else {
                    false
                }
                // shard lock released here
            };
            if pinned {
                // We hold a confirmed pin.  The frame must not be EVICTED while
                // the pin is outstanding — this is the use-after-free guard.
                assert_ne!(
                    p_state.load(Ordering::Acquire),
                    EVICTED,
                    "frame evicted while a pin taken under the map lock was held"
                );
                p_pin.fetch_sub(1, Ordering::Relaxed);
            }
            pinned
        });

        // Evictor: take the shard lock, recheck the pin under it, and only
        // commit the eviction (unmap + state=EVICTED) when no pin is held.
        let e_shard = Arc::clone(&shard);
        let e_pin = Arc::clone(&pin_count);
        let e_state = Arc::clone(&state);
        let evictor = loom::thread::spawn(move || {
            let mut mapped = e_shard.lock().unwrap();
            // Recheck the pin under the shard lock (the linearisation point).
            if e_pin.load(Ordering::Relaxed) != 0 {
                return false; // pinned: abort, leave the frame mapped & Resident.
            }
            // Commit: unmap the page and mark the frame evicted, both under lock.
            *mapped = false;
            e_state.store(EVICTED, Ordering::Release);
            true
        });

        let _ = pinner.join().unwrap();
        let _ = evictor.join().unwrap();

        // Schedule-independent post-condition: the frame state and the map cell
        // agree — a frame is EVICTED iff it was unmapped, never half-way.
        let final_mapped = *shard.lock().unwrap();
        let final_state = state.load(Ordering::Acquire);
        assert_eq!(
            final_state == EVICTED,
            !final_mapped,
            "frame state and shard-map mapping diverged (torn eviction)"
        );
    });
}
