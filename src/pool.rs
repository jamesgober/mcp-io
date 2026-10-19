//! A fixed set of worker threads that run tool calls off the connection's
//! reading thread.
//!
//! The connection reads and dispatches on one thread. If a tool ran there,
//! nothing else could be read while it ran, so a `notifications/cancelled`
//! for it would wait until it had finished. Tool calls therefore go to a
//! bounded pool: `workers` threads, and at most `queue` calls waiting for one
//! (more are refused with a JSON-RPC error rather than queued without bound).
//! With zero workers, calls run inline on the reading thread (deterministic,
//! for tests and tiny servers; cancellation then cannot interrupt a call).

use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

type Job = Box<dyn FnOnce() + Send>;

/// Each worker's stack: 8 MiB, Linux's default main-thread size, rather than
/// Rust's 2 MiB for spawned threads.
const WORKER_STACK: usize = 8 << 20;

/// Calls submitted and not yet finished, and the bytes they hold, with a
/// condition variable to wait for zero.
struct Load {
    /// `(calls, bytes)`.
    count: Mutex<(usize, usize)>,
    idle: Condvar,
}

impl Load {
    fn finish_one(&self, bytes: usize) {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        count.0 = count.0.saturating_sub(1);
        count.1 = count.1.saturating_sub(bytes);
        if count.0 == 0 {
            self.idle.notify_all();
        }
    }
}

pub(crate) struct Pool {
    sender: Option<mpsc::Sender<Job>>,
    load: Arc<Load>,
    limit: usize,
    byte_limit: usize,
}

impl Pool {
    /// Starts `workers` threads; at most `limit` calls, holding at most
    /// `byte_limit` bytes between them, may be in the pool (running or
    /// waiting) at once. A call larger than `byte_limit` is still taken
    /// into an empty pool.
    pub(crate) fn new(workers: usize, limit: usize, byte_limit: usize) -> io::Result<Self> {
        let load = Arc::new(Load {
            count: Mutex::new((0, 0)),
            idle: Condvar::new(),
        });
        let sender = if workers == 0 {
            None
        } else {
            let (sender, receiver) = mpsc::channel::<Job>();
            let receiver = Arc::new(Mutex::new(receiver));
            for n in 0..workers {
                let receiver = Arc::clone(&receiver);
                let _detached = thread::Builder::new()
                    .name(format!("mcp-io-worker-{n}"))
                    // Room for deep validation and for handlers.
                    .stack_size(WORKER_STACK)
                    .spawn(move || worker(&receiver))?;
            }
            Some(sender)
        };
        Ok(Self {
            sender,
            load,
            limit: limit.max(1),
            byte_limit,
        })
    }

    /// Whether a call holding `bytes` would be taken now.
    pub(crate) fn has_room(&self, bytes: usize) -> bool {
        let count = self
            .load
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        count.0 < self.limit && (count.0 == 0 || count.1.saturating_add(bytes) <= self.byte_limit)
    }

    /// Runs `job`, which holds `bytes`, on a worker (or inline, with no
    /// workers). `Err(job)` when the pool is full; the caller refuses the
    /// call.
    pub(crate) fn submit<F: FnOnce() + Send + 'static>(
        &self,
        bytes: usize,
        job: F,
    ) -> Result<(), F> {
        {
            let mut count = self
                .load
                .count
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let full = count.0 >= self.limit
                || (count.0 > 0 && count.1.saturating_add(bytes) > self.byte_limit);
            if full {
                return Err(job);
            }
            count.0 += 1;
            count.1 = count.1.saturating_add(bytes);
        }
        let load = Arc::clone(&self.load);
        let wrapped: Job = Box::new(move || {
            // The job contains its own panics; this guard keeps the count
            // right even if one escapes.
            let _contained = catch_unwind(AssertUnwindSafe(job));
            load.finish_one(bytes);
        });
        match &self.sender {
            None => wrapped(),
            Some(sender) => {
                if let Err(mpsc::SendError(job)) = sender.send(wrapped) {
                    // Every worker is gone (none can be, while the pool
                    // lives); run it here rather than lose it.
                    job();
                }
            }
        }
        Ok(())
    }

    /// Calls in the pool now.
    #[cfg(test)]
    pub(crate) fn load(&self) -> usize {
        self.load
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Waits until no call is in the pool, for at most `timeout`. Whether it
    /// emptied.
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut count = self
            .load
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while count.0 > 0 {
            let wait = match deadline {
                None => Duration::from_secs(3600),
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
            };
            count = self
                .load
                .idle
                .wait_timeout(count, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

fn worker(receiver: &Mutex<mpsc::Receiver<Job>>) {
    loop {
        // Hold the lock only to take one job, never while running it.
        let job = receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv();
        match job {
            Ok(job) => job(),
            // The pool was dropped: no more jobs will come.
            Err(mpsc::RecvError) => return,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn test_inline_pool_runs_at_once() {
        let pool = Pool::new(0, 4, 1 << 20).unwrap();
        assert!(pool.sender.is_none());
        let ran = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&ran);
        assert!(
            pool.submit(1, move || {
                let _previous = seen.fetch_add(1, Ordering::SeqCst);
            })
            .is_ok()
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(pool.load(), 0);
        assert!(pool.wait_idle(Duration::ZERO));
    }

    #[test]
    fn test_pool_refuses_past_its_limit_and_drains() {
        let pool = Pool::new(1, 2, 1 << 20).unwrap();
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            assert!(
                pool.submit(1, move || {
                    let _ = gate.lock().map(|g| g.recv());
                })
                .is_ok()
            );
        }
        assert!(
            pool.submit(1, || {}).is_err(),
            "the third call is over the limit"
        );
        assert!(!pool.wait_idle(Duration::from_millis(10)));
        for _ in 0..2 {
            assert!(release.send(()).is_ok());
        }
        assert!(pool.wait_idle(Duration::from_secs(30)));
        assert_eq!(pool.load(), 0);
    }

    #[test]
    fn test_pool_refuses_past_its_byte_limit_but_takes_one_large_call() {
        let pool = Pool::new(1, 8, 100).unwrap();
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let waiting = Arc::clone(&gate);
        assert!(pool.has_room(1000), "an empty pool takes anything");
        assert!(
            pool.submit(1000, move || {
                let _ = waiting.lock().map(|g| g.recv());
            })
            .is_ok()
        );
        assert!(!pool.has_room(1));
        assert!(pool.submit(1, || {}).is_err());
        assert!(release.send(()).is_ok());
        assert!(pool.wait_idle(Duration::from_secs(30)));
        assert!(pool.has_room(60) && pool.submit(60, || {}).is_ok());
        assert!(pool.wait_idle(Duration::from_secs(30)));
    }

    #[test]
    fn test_a_panicking_job_does_not_kill_its_worker() {
        let pool = Pool::new(1, 8, 1 << 20).unwrap();
        assert!(pool.submit(1, || std::panic::panic_any("boom")).is_ok());
        let ran = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&ran);
        assert!(
            pool.submit(1, move || {
                let _previous = seen.fetch_add(1, Ordering::SeqCst);
            })
            .is_ok()
        );
        assert!(pool.wait_idle(Duration::from_secs(30)));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
