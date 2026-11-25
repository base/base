//! Worker threads for state roots, started once and kept, so a root does not spawn threads:
//! new threads on a busy machine wait their turn behind everything else running, and a root
//! split over freshly spawned threads finishes only when the last of them has been scheduled.
//!
//! [`WorkerPool::run`] splits jobs between the calling thread and idle workers that claim them
//! one at a time; it waits only for jobs a worker has claimed, never for a worker to wake up, so
//! when no worker gets to run the calling thread does every job itself.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::TrieError;

/// Worker threads the pool starts.
const POOL_WORKERS: usize = 16;

type Task = Box<dyn FnOnce() + Send>;

/// A thread's context for jobs, opened once per thread and batch.
type InitFn<C> = Box<dyn Fn() -> Result<C, TrieError> + Send + Sync>;

/// One job by index, with the thread's context.
type JobFn<C, R> = Box<dyn Fn(&mut C, usize) -> Result<R, TrieError> + Send + Sync>;

/// The process's root worker threads (see the module docs).
pub struct WorkerPool {
    queue: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool").finish_non_exhaustive()
    }
}

/// One [`WorkerPool::run`]: its jobs, their results, and how many are left.
struct Batch<C, R> {
    jobs: usize,
    next: AtomicUsize,
    results: Mutex<Vec<Option<Result<R, TrieError>>>>,
    left: Mutex<usize>,
    done: Condvar,
    init: InitFn<C>,
    job: JobFn<C, R>,
}

impl<C, R> Batch<C, R> {
    /// Claims and runs jobs until none are left unclaimed, with a context opened on first use.
    fn work(&self) {
        let mut context = None;
        loop {
            let index = self.next.fetch_add(1, Ordering::Relaxed);
            if index >= self.jobs {
                return;
            }
            let result = match context.get_or_insert_with(|| (self.init)()) {
                Ok(context) => (self.job)(context, index),
                Err(error) => Err(TrieError::Unknown(format!("{error}"))),
            };
            self.results.lock().expect("unpoisoned")[index] = Some(result);
            let mut left = self.left.lock().expect("unpoisoned");
            *left -= 1;
            if *left == 0 {
                self.done.notify_all();
            }
        }
    }
}

impl WorkerPool {
    /// The process's pool, started on first use.
    pub fn global() -> &'static Self {
        static POOL: OnceLock<Arc<WorkerPool>> = OnceLock::new();
        POOL.get_or_init(|| {
            let pool = Arc::new(Self { queue: Mutex::default(), ready: Condvar::new() });
            for n in 0..POOL_WORKERS {
                let worker = Arc::clone(&pool);
                // A pool that could not start a worker still runs every job on the caller.
                let _ = std::thread::Builder::new()
                    .name(format!("root-worker-{n}"))
                    .spawn(move || worker.serve());
            }
            pool
        })
    }

    fn serve(&self) {
        loop {
            let task = {
                let mut queue = self.queue.lock().expect("unpoisoned");
                loop {
                    match queue.pop_front() {
                        Some(task) => break task,
                        None => queue = self.ready.wait(queue).expect("unpoisoned"),
                    }
                }
            };
            task();
        }
    }

    /// Runs `job(context, index)` for every index below `jobs`, on this thread and up to
    /// `helpers` workers, each with its own context from `init`; the results are in index
    /// order, or the first error.
    pub fn run<C: 'static, R: Send + 'static>(
        &self,
        jobs: usize,
        helpers: usize,
        init: impl Fn() -> Result<C, TrieError> + Send + Sync + 'static,
        job: impl Fn(&mut C, usize) -> Result<R, TrieError> + Send + Sync + 'static,
    ) -> Result<Vec<R>, TrieError> {
        let batch = Arc::new(Batch {
            jobs,
            next: AtomicUsize::new(0),
            results: Mutex::new((0..jobs).map(|_| None).collect()),
            left: Mutex::new(jobs),
            done: Condvar::new(),
            init: Box::new(init),
            job: Box::new(job),
        });
        {
            let mut queue = self.queue.lock().expect("unpoisoned");
            for _ in 0..helpers.min(jobs.saturating_sub(1)) {
                let batch = Arc::clone(&batch);
                queue.push_back(Box::new(move || batch.work()));
            }
        }
        self.ready.notify_all();
        batch.work();
        let mut left = batch.left.lock().expect("unpoisoned");
        while *left > 0 {
            left = batch.done.wait(left).expect("unpoisoned");
        }
        drop(left);
        let results = std::mem::take(&mut *batch.results.lock().expect("unpoisoned"));
        results.into_iter().map(|result| result.expect("every job ran")).collect()
    }
}
