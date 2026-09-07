//! A simple trait to allow generic executors or wrappers for spawning the discv5 tasks.
use std::{future::Future, pin::Pin};

pub trait Executor: ExecutorClone {
    /// Run the given future in the background until it ends.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>);
}

pub trait ExecutorClone {
    fn clone_box(&self) -> Box<dyn Executor + Send + Sync>;
}

impl<T> ExecutorClone for T
where
    T: 'static + Executor + Clone + Send + Sync,
{
    fn clone_box(&self) -> Box<dyn Executor + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn Executor + Send + Sync> {
    fn clone(&self) -> Box<dyn Executor + Send + Sync> {
        self.clone_box()
    }
}

#[derive(Clone)]
pub struct TokioExecutor;

impl Executor for TokioExecutor {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        tokio::task::spawn(future);
    }
}

impl Default for TokioExecutor {
    fn default() -> Self {
        TokioExecutor
    }
}
