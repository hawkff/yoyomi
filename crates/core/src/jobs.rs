use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;

use parking_lot::{Condvar, Mutex};

use crate::Cancel;

/// How soon a job should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Work nobody waits for, such as a background search.
    Idle,
    /// The pages around the one on screen.
    Neighbor,
    /// What the screen shows next.
    Visible,
}

/// A pool of worker threads that run jobs by priority, oldest first within a
/// priority. A job whose token fires before the job starts never runs. Clones
/// share the pool, and the workers stop once the last clone drops. Jobs report
/// their results through channels they capture.
#[derive(Clone)]
pub struct Jobs {
    handle: Arc<Handle>,
}

/// The callers' share of a pool. Workers hold only `Shared`, so the pool
/// closes when the last caller lets go of its handle.
struct Handle {
    shared: Arc<Shared>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.close();
    }
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

impl Shared {
    fn close(&self) {
        let queued = {
            let mut queue = self.queue.lock();
            queue.closed = true;
            std::mem::take(&mut queue.heap)
        };
        self.ready.notify_all();
        // Dropping a job can drop what it captured, even a `Jobs` handle, so
        // it happens outside the lock.
        drop(queued);
    }
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Job>,
    next: u64,
    closed: bool,
}

struct Job {
    priority: Priority,
    seq: u64,
    cancel: Cancel,
    run: Box<dyn FnOnce() + Send>,
}

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Eq for Job {}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Job {
    // The heap pops the greatest job: the highest priority, then the oldest.
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.cmp(&other.priority).then_with(|| other.seq.cmp(&self.seq))
    }
}

impl Jobs {
    /// Starts `threads` workers, at least one.
    pub fn new(threads: usize) -> Self {
        let shared = Arc::new(Shared { queue: Mutex::new(Queue::default()), ready: Condvar::new() });
        for i in 0..threads.max(1) {
            let shared = shared.clone();
            thread::Builder::new().name(format!("yoyomi-job-{i}")).spawn(move || work(&shared)).expect("start a job thread");
        }
        Jobs { handle: Arc::new(Handle { shared }) }
    }

    /// A pool sized for this machine: one worker per core, from two to four.
    pub fn for_machine() -> Self {
        let cores = thread::available_parallelism().map_or(2, |n| n.get());
        Self::new(cores.clamp(2, 4))
    }

    /// Queues `job`. A worker runs it unless `cancel` fires first.
    pub fn spawn(&self, priority: Priority, cancel: &Cancel, job: impl FnOnce() + Send + 'static) {
        let shared = &self.handle.shared;
        let mut queue = shared.queue.lock();
        if queue.closed {
            drop(queue);
            return;
        }
        let seq = queue.next;
        queue.next += 1;
        queue.heap.push(Job { priority, seq, cancel: cancel.clone(), run: Box::new(job) });
        drop(queue);
        shared.ready.notify_one();
    }

    /// Drops the queued jobs and stops each worker once its current job ends.
    /// Dropping the last clone does the same.
    pub fn shutdown(&self) {
        self.handle.shared.close();
    }
}

fn work(shared: &Shared) {
    loop {
        let job = {
            let mut queue = shared.queue.lock();
            loop {
                if queue.closed {
                    return;
                }
                if let Some(job) = queue.heap.pop() {
                    break job;
                }
                shared.ready.wait(&mut queue);
            }
        };
        if !job.cancel.is_cancelled() {
            // A panicking job must not take its worker down with it.
            let _ = catch_unwind(AssertUnwindSafe(job.run));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn runs_by_priority_then_age() {
        let jobs = Jobs::new(1);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        let cancel = Cancel::new();
        // Hold the only worker until every job is queued.
        jobs.spawn(Priority::Visible, &cancel, move || gate_rx.recv().unwrap());
        thread::sleep(Duration::from_millis(50));
        for (priority, name) in
            [(Priority::Idle, "idle"), (Priority::Neighbor, "next"), (Priority::Visible, "shown"), (Priority::Neighbor, "previous")]
        {
            let done = done_tx.clone();
            jobs.spawn(priority, &cancel, move || done.send(name).unwrap());
        }
        gate_tx.send(()).unwrap();
        let order: Vec<_> = (0..4).map(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(order, ["shown", "next", "previous", "idle"]);
        jobs.shutdown();
    }

    #[test]
    fn a_cancelled_job_never_runs() {
        let jobs = Jobs::new(1);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        jobs.spawn(Priority::Visible, &Cancel::new(), move || gate_rx.recv().unwrap());
        thread::sleep(Duration::from_millis(50));
        let stale = Cancel::new();
        let done = done_tx.clone();
        jobs.spawn(Priority::Visible, &stale, move || done.send("stale").unwrap());
        jobs.spawn(Priority::Idle, &Cancel::new(), move || done_tx.send("fresh").unwrap());
        stale.cancel();
        gate_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "fresh");
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        jobs.shutdown();
    }

    #[test]
    fn the_workers_stop_with_the_last_handle() {
        let jobs = Jobs::new(2);
        let workers = Arc::downgrade(&jobs.handle.shared);
        let clone = jobs.clone();
        drop(jobs);
        let (done_tx, done_rx) = mpsc::channel();
        clone.spawn(Priority::Visible, &Cancel::new(), move || done_tx.send(()).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "a clone keeps the pool open");
        drop(clone);
        for _ in 0..100 {
            if workers.strong_count() == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the workers outlived the last handle");
    }

    #[test]
    fn a_job_holding_the_last_handle_drops_without_deadlock() {
        let jobs = Jobs::new(1);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        jobs.spawn(Priority::Visible, &Cancel::new(), move || gate_rx.recv().unwrap());
        thread::sleep(Duration::from_millis(50));
        let captured = jobs.clone();
        jobs.spawn(Priority::Idle, &Cancel::new(), move || drop(captured));
        // The queued job holds the other handle; dropping it during shutdown
        // must not wait for the queue lock that shutdown holds.
        drop(jobs);
        gate_tx.send(()).unwrap_or(());
    }

    #[test]
    fn a_panicking_job_keeps_the_worker() {
        let jobs = Jobs::new(1);
        let (done_tx, done_rx) = mpsc::channel();
        jobs.spawn(Priority::Visible, &Cancel::new(), || panic!("job failed"));
        jobs.spawn(Priority::Idle, &Cancel::new(), move || done_tx.send(()).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        jobs.shutdown();
    }
}
