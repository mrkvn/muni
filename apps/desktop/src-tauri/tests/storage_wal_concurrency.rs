//! Plan 039 task 47 — proves the WAL-mode fix actually delivers the
//! property the plan cites `wal.html` for: an insert from one writer
//! connection succeeds while another writer connection is mid-`VACUUM`
//! against the same `history.sqlite3` file.
//!
//! Before the fix, `history.sqlite3` connections used SQLite's default
//! rollback-journal mode with no `busy_timeout` configured. `VACUUM` takes
//! an exclusive file lock for its *entire* duration in that mode, and a
//! second connection attempting to write with no timeout gets an immediate
//! `SQLITE_BUSY` — it does not wait. WAL mode plus a generous
//! `busy_timeout` (both applied by `configure_shared_writer_connection`,
//! wired into every store's `open()`) lets the second writer's operation
//! simply wait out the brief windows where they'd otherwise collide, so the
//! insert succeeds instead of erroring.

use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use muni_lib::history_store::{
    configure_shared_writer_connection, HistoryStore, NewDictationRecord, SERVED_BY_GLADIA_PRIMARY,
};
use rusqlite::Connection;
use tempfile::tempdir;

/// Insert enough padding rows that `VACUUM` (via `wipe_all`) has real (if
/// small) work to do, widening the window in which a genuinely concurrent
/// write could land.
const PADDING_ROWS: usize = 500;

#[test]
fn insert_survives_concurrent_vacuum_under_wal() {
    let dir = tempdir().expect("tempdir");
    let path = HistoryStore::default_path(dir.path());

    // Two independent connections onto the same file — this is exactly the
    // "three writer connections on one DB file" shape the plan describes
    // (here, two `HistoryStore` handles standing in for two of the three
    // production writers, which all share the identical
    // `configure_shared_writer_connection` setup applied in `open()`).
    let vacuum_store = Arc::new(HistoryStore::open(&path).expect("open vacuum store"));
    let insert_store = Arc::new(HistoryStore::open(&path).expect("open insert store"));

    for i in 0..PADDING_ROWS {
        vacuum_store
            .insert(NewDictationRecord {
                raw_text: format!(
                    "raw padding row {i} with some extra bytes to bulk up the page count"
                ),
                cleaned_text: format!(
                    "padding row {i} with some extra bytes to bulk up the page count"
                ),
                target_app_bundle_id: None,
                served_by: SERVED_BY_GLADIA_PRIMARY.into(),
            })
            .expect("insert padding row");
    }

    // Rendezvous so both threads start their operation at nearly the same
    // instant, maximizing the chance of a genuine overlap between the
    // VACUUM (triggered by `wipe_all`) and the concurrent insert.
    let barrier = Arc::new(Barrier::new(2));

    let vacuum_thread = {
        let store = Arc::clone(&vacuum_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            // `wipe_all` is DELETE-then-VACUUM (see history_store.rs) —
            // the public surface for exercising a real VACUUM against this
            // file without reaching into the store's private connection.
            store.wipe_all()
        })
    };

    let insert_thread = {
        let store = Arc::clone(&insert_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.insert(NewDictationRecord {
                raw_text: "raw concurrent insert".into(),
                cleaned_text: "concurrent insert".into(),
                target_app_bundle_id: None,
                served_by: SERVED_BY_GLADIA_PRIMARY.into(),
            })
        })
    };

    // Bound the wait so a real regression (a hang, not just an error) fails
    // the test instead of stalling CI forever.
    let vacuum_result = join_with_timeout(vacuum_thread, Duration::from_secs(10));
    let insert_result = join_with_timeout(insert_thread, Duration::from_secs(10));

    assert!(
        vacuum_result.is_ok(),
        "wipe_all/VACUUM must succeed: {vacuum_result:?}"
    );
    assert!(
        insert_result.is_ok(),
        "insert during concurrent VACUUM must succeed under WAL, got: {insert_result:?}"
    );
    assert!(insert_result.unwrap() > 0);

    // Data integrity. `wipe_all` is DELETE-then-VACUUM, and the concurrent
    // insert races that DELETE: if the insert committed BEFORE the DELETE, the
    // DELETE legitimately wipes its row too (0 rows left); if AFTER, it survives
    // (1 row). BOTH are correct outcomes of a genuine overlap — the property
    // under test is that the insert did not error with SQLITE_BUSY (asserted
    // above via `insert_result.is_ok()` + `unwrap() > 0`), NOT that it won the
    // race against the DELETE. Asserting exactly 1 row made this flaky (~20%
    // locally, and a CI red) because it encoded one arbitrary race outcome. So:
    // at most the single concurrent row can remain, and if it did, it's intact.
    let rows = insert_store.list(None).unwrap();
    assert!(
        rows.len() <= 1,
        "only the concurrent insert can outlive wipe_all's DELETE; got {} rows",
        rows.len()
    );
    if let Some(row) = rows.first() {
        assert_eq!(row.cleaned_text, "concurrent insert");
    }
}

/// Deterministic sibling of [`insert_survives_concurrent_vacuum_under_wal`]:
/// rather than racing a real `VACUUM` against the clock (which can pass
/// vacuously if the padding DB is small enough that `VACUUM` finishes before
/// the concurrent insert even attempts its write), this forces a genuine
/// cross-connection lock with an explicit `BEGIN IMMEDIATE` held for a fixed
/// window, then asserts the second connection's insert *waits out* that
/// window (via `busy_timeout`) rather than failing with `SQLITE_BUSY`
/// immediately — the exact failure mode `configure_shared_writer_connection`
/// exists to prevent. Uses a raw `Connection` (not `HistoryStore`) for the
/// lock-holder so the test controls precisely how long the lock is held.
#[test]
fn concurrent_writer_waits_out_lock_instead_of_erroring_busy() {
    let dir = tempdir().expect("tempdir");
    let path = HistoryStore::default_path(dir.path());
    // Opening the store first creates the schema and switches the file to
    // WAL — the raw lock-holder connection below inherits that from the
    // file header.
    let insert_store = HistoryStore::open(&path).expect("open store");

    let barrier = Arc::new(Barrier::new(2));
    let lock_thread = {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let conn = Connection::open(&path).expect("raw open");
            configure_shared_writer_connection(&conn).expect("configure raw conn");
            conn.execute("BEGIN IMMEDIATE", [])
                .expect("acquire write lock");
            // Only signal readiness AFTER the lock is actually held, so the
            // main thread's insert attempt below is guaranteed to contend
            // for it rather than racing to start first.
            barrier.wait();
            thread::sleep(Duration::from_millis(400));
            conn.execute("COMMIT", []).expect("release write lock");
        })
    };

    barrier.wait();
    let started = Instant::now();
    let id = insert_store
        .insert(NewDictationRecord {
            raw_text: "raw contended insert".into(),
            cleaned_text: "contended insert".into(),
            target_app_bundle_id: None,
            served_by: SERVED_BY_GLADIA_PRIMARY.into(),
        })
        .expect("insert must wait out the lock and succeed, not error SQLITE_BUSY");
    let waited = started.elapsed();
    assert!(id > 0);
    // Prove the insert actually contended for the lock rather than getting
    // lucky and landing before `BEGIN IMMEDIATE` — without `busy_timeout`
    // this assertion (and the `.expect` above) is what a regression breaks.
    assert!(
        waited >= Duration::from_millis(200),
        "insert returned in {waited:?} — too fast to have waited out the held lock; \
         is busy_timeout actually configured?"
    );

    join_with_timeout(lock_thread, Duration::from_secs(5));
}

/// Join a thread with a bounded wait, panicking (failing the test) instead
/// of hanging forever if the underlying operation never returns — a hang
/// (not just a `SQLITE_BUSY` error) is exactly the failure mode a
/// misconfigured `busy_timeout` could reintroduce.
fn join_with_timeout<T: Send + 'static>(handle: thread::JoinHandle<T>, timeout: Duration) -> T {
    let (tx, rx) = mpsc::channel();
    // A detached reaper thread: it outlives this function if `recv_timeout`
    // fires first, but that's a bounded, test-process-local leak — the
    // process exits at the end of the test binary regardless.
    thread::spawn(move || {
        let result = handle.join();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!(
            "operation did not complete within {timeout:?} — likely a WAL/busy_timeout regression"
        ),
    }
}
