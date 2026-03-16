//! Internal executor state: task queue, waker, virtual clock, and sleep alarm.

use std::{
    collections::{BTreeMap, BinaryHeap},
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task,
    task::{Poll, Waker},
    time::Duration,
};

use futures::task::ArcWake;
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};

/// Core executor state shared across all components.
#[derive(Debug)]
pub struct Executor {
    /// Current virtual time. Only advanced by `skip_idle_time`.
    pub time: Mutex<Duration>,
    /// All live tasks, keyed by monotonic ID.
    pub tasks: Arc<Tasks>,
    /// Registered sleep alarms, stored as a min-heap (earliest deadline first).
    sleeping: Mutex<BinaryHeap<Alarm>>,
    /// Seeded RNG used to shuffle the ready queue before each polling round.
    rng: Mutex<StdRng>,
}

impl Executor {
    /// Create a new executor with a seeded RNG for deterministic task scheduling.
    pub fn new(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            time: Mutex::new(Duration::ZERO),
            tasks: Arc::new(Tasks::default()),
            sleeping: Mutex::new(BinaryHeap::new()),
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
        })
    }

    /// Drain the ready queue, shuffle it with the seeded RNG, and poll each task.
    ///
    /// Tasks that complete are removed from `running`. Tasks that are still
    /// pending remain registered via their waker.
    pub fn poll_ready(self: &Arc<Self>) {
        let mut queue = self.tasks.drain();
        if queue.len() > 1 {
            queue.shuffle(&mut *self.rng.lock().unwrap());
        }
        for id in queue {
            let task = self.tasks.running.lock().unwrap().get(&id).cloned();
            if let Some(task) = task {
                let waker = futures::task::waker(Arc::new(TaskWaker {
                    id,
                    tasks: Arc::downgrade(&self.tasks),
                }));
                let mut cx = task::Context::from_waker(&waker);
                let done = task.future.lock().unwrap().as_mut().poll(&mut cx).is_ready();
                if done {
                    self.tasks.running.lock().unwrap().remove(&id);
                }
            }
        }
    }

    /// If no tasks are ready, jump virtual time to the next alarm deadline.
    pub fn skip_idle_time(&self) {
        if !self.tasks.has_ready()
            && let Some(alarm) = self.sleeping.lock().unwrap().peek()
        {
            let mut time = self.time.lock().unwrap();
            if alarm.time > *time {
                *time = alarm.time;
            }
        }
    }

    /// Wake every sleeper whose deadline is at or before the current virtual time.
    pub fn wake_ready_sleepers(&self) {
        let now = *self.time.lock().unwrap();
        let mut sleeping = self.sleeping.lock().unwrap();
        while sleeping.peek().is_some_and(|a| a.time <= now) {
            sleeping.pop().unwrap().waker.wake();
        }
    }

    /// Panic if there are no ready tasks and no pending sleepers (deadlock).
    pub fn assert_liveness(&self) {
        if !self.tasks.has_ready() && self.sleeping.lock().unwrap().is_empty() {
            panic!("runtime stalled: no ready tasks and no pending sleepers");
        }
    }
}

/// Task queue: counter, ready set, and running map.
#[derive(Debug)]
pub struct Tasks {
    counter: Mutex<u128>,
    ready: Mutex<Vec<u128>>,
    /// All live tasks, keyed by monotonic ID.
    pub running: Mutex<BTreeMap<u128, Arc<Task>>>,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            counter: Mutex::new(0),
            ready: Mutex::new(Vec::new()),
            running: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Tasks {
    /// Insert a new task into the running map and push it onto the ready queue.
    pub fn insert(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        let id = {
            let mut c = self.counter.lock().unwrap();
            let id = *c;
            *c += 1;
            id
        };
        self.running.lock().unwrap().insert(id, Arc::new(Task { future: Mutex::new(future) }));
        self.queue(id);
    }

    /// Push a task ID onto the ready queue (called by `TaskWaker`).
    pub fn queue(&self, id: u128) {
        self.ready.lock().unwrap().push(id);
    }

    /// Drain the entire ready queue into a `Vec` for the current polling round.
    pub fn drain(&self) -> Vec<u128> {
        std::mem::take(&mut *self.ready.lock().unwrap())
    }

    /// Returns `true` if there is at least one task waiting to be polled.
    pub fn has_ready(&self) -> bool {
        !self.ready.lock().unwrap().is_empty()
    }
}

/// A single boxed future with its execution state.
pub struct Task {
    /// The pinned future being driven to completion.
    pub future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").finish_non_exhaustive()
    }
}

/// Waker that re-queues a task ID when woken.
struct TaskWaker {
    id: u128,
    tasks: Weak<Tasks>,
}

impl ArcWake for TaskWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if let Some(tasks) = arc_self.tasks.upgrade() {
            tasks.queue(arc_self.id);
        }
    }
}

/// A registered sleep alarm stored in the `BinaryHeap`.
///
/// `Ord` is reversed so the heap acts as a min-heap: the earliest deadline
/// is always at the top.
#[derive(Debug)]
pub struct Alarm {
    /// The virtual time at which this alarm fires.
    pub time: Duration,
    /// Waker to notify when the alarm fires.
    pub waker: Waker,
}

impl PartialEq for Alarm {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl Eq for Alarm {}

impl PartialOrd for Alarm {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Alarm {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.time.cmp(&self.time)
    }
}

/// A future that resolves when virtual time reaches `deadline`.
///
/// On first poll, registers an [`Alarm`] in the executor's sleep heap. The
/// executor calls `skip_idle_time` + `wake_ready_sleepers` to advance virtual
/// time and wake this future without spinning.
#[derive(Debug)]
pub struct Sleeper {
    /// Weak reference to the executor managing virtual time.
    pub executor: Weak<Executor>,
    /// The virtual time at which this sleeper should wake.
    pub deadline: Duration,
    /// Whether this sleeper has already registered its alarm.
    pub registered: bool,
}

impl Future for Sleeper {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let Some(executor) = self.executor.upgrade() else {
            return Poll::Ready(());
        };
        if *executor.time.lock().unwrap() >= self.deadline {
            return Poll::Ready(());
        }
        if !self.registered {
            self.registered = true;
            // SAFETY: The waker is not updated on subsequent polls. This is
            // sound because every TaskWaker for a given task ID has identical
            // behaviour (tasks.queue(id)), so waking via a waker from any
            // earlier poll has the same effect as waking via the most recent
            // one. Updating the waker in-place is also not possible with
            // BinaryHeap, which provides no random-access mutation.
            executor
                .sleeping
                .lock()
                .unwrap()
                .push(Alarm { time: self.deadline, waker: cx.waker().clone() });
        }
        Poll::Pending
    }
}
