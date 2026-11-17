//! Owns the SQLite store on a dedicated thread. Nothing here ever blocks the async runtime: admission is a
//! non-blocking send on an unbounded channel. Bulk writes (transcript appends, retags) are capped by an
//! in-flight counter and dropped (counted, reported) past it; critical writes and calls are low volume
//! (bounded by the job queue) and always admitted.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use tokio::sync::oneshot;
use zora_core::store::Store;

type Job = Box<dyn FnOnce(&mut Store) + Send>;

/// Maximum transcript writes waiting for a slow disk.
const BULK_BACKLOG: usize = 2048;

#[derive(Clone)]
pub struct StoreHandle {
    tx: mpsc::Sender<Job>,
    bulk_in_flight: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    /// Command records admitted but whose terminal status write has not yet run on the store thread.
    records: Arc<AtomicUsize>,
}

impl StoreHandle {
    pub fn open(path: &Path) -> Result<StoreHandle, String> {
        let store = Store::open(path, false)?;
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("zora-store".into())
            .spawn(move || {
                let mut store = store;
                for job in rx {
                    job(&mut store);
                }
                let _ = store.flush();
            })
            .map_err(|e| e.to_string())?;
        Ok(StoreHandle {
            tx,
            bulk_in_flight: Default::default(),
            dropped: Default::default(),
            records: Default::default(),
        })
    }

    /// Transcript writes: dropped (and counted) when the disk has fallen too far behind.
    pub fn send(&self, f: impl FnOnce(&mut Store) + Send + 'static) {
        if self.bulk_in_flight.fetch_add(1, Ordering::AcqRel) >= BULK_BACKLOG {
            self.bulk_in_flight.fetch_sub(1, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let counter = self.bulk_in_flight.clone();
        let job: Job = Box::new(move |s| {
            f(s);
            counter.fetch_sub(1, Ordering::AcqRel);
        });
        if self.tx.send(job).is_err() {
            self.bulk_in_flight.fetch_sub(1, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Writes whose loss is not safe (job lifecycle). Always admitted, never blocks.
    pub fn send_critical(&self, f: impl FnOnce(&mut Store) + Send + 'static) {
        let _ = self.tx.send(Box::new(f));
    }

    /// Reserves room for one command record until its terminal write has actually been stored. Critical store
    /// traffic is therefore bounded even when jobs finish instantly during a disk stall.
    pub fn try_reserve_record(&self, limit: usize) -> bool {
        if self.records.fetch_add(1, Ordering::AcqRel) >= limit {
            self.records.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// The terminal write for a reserved record; the reservation is released once it has run.
    pub fn send_terminal(&self, f: impl FnOnce(&mut Store) + Send + 'static) {
        let records = self.records.clone();
        let job: Job = Box::new(move |s| {
            f(s);
            records.fetch_sub(1, Ordering::AcqRel);
        });
        if self.tx.send(job).is_err() {
            self.records.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Number of writes dropped since the last call.
    pub fn take_dropped(&self) -> usize {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Run on the store thread and await the result (admission never blocks the runtime).
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |s| {
                let _ = tx.send(f(s));
            }))
            .ok()?;
        rx.await.ok()
    }
}
