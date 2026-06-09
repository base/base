//! Native precompile observation hooks.

use crate::{PrecompileCallMetric, PrecompileCallOutcome};

/// Observer that does not record native precompile calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPrecompileCallObserver;

/// Observer for native precompile call execution.
pub trait PrecompileCallObserver: Clone + Send + Sync + 'static {
    /// Called before executing a labeled precompile operation.
    fn start(&self, _label: &'static str) {}

    /// Called after executing a labeled precompile operation.
    fn end(&self, _label: &'static str) {}

    /// Executes `f` between the observer's start and end hooks.
    fn observe<R>(&self, label: &'static str, f: impl FnOnce() -> R) -> R
    where
        Self: Sized,
    {
        self.start(label);
        let _guard = EndGuard { observer: self, label };
        f()
    }

    /// Records one completed native precompile call.
    fn record_call(&self, _call: &PrecompileCallMetric, _outcome: &PrecompileCallOutcome) {}

    /// Records a B-20 token creation.
    fn record_b20_created(&self, _variant: &'static str) {}

    /// Records the number of logical items in a Beryl batch call.
    fn record_batch_items(&self, _call: &PrecompileCallMetric, _count: usize) {}

    /// Records internal calls made by Beryl precompile logic.
    fn record_internal_calls(&self, _call: &PrecompileCallMetric, _calls: usize, _bytes: usize) {}
}

impl PrecompileCallObserver for NoopPrecompileCallObserver {}

/// Guard that calls [`PrecompileCallObserver::end`] when observed work finishes.
#[derive(Debug)]
pub struct EndGuard<'a, O>
where
    O: PrecompileCallObserver,
{
    observer: &'a O,
    label: &'static str,
}

impl<O> Drop for EndGuard<'_, O>
where
    O: PrecompileCallObserver,
{
    fn drop(&mut self) {
        self.observer.end(self.label);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{Arc, Mutex},
    };

    use crate::PrecompileCallObserver;

    #[derive(Debug, Clone)]
    struct RecordingObserver {
        events: Arc<Mutex<Vec<(&'static str, &'static str)>>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self { events: Arc::new(Mutex::new(Vec::new())) }
        }

        fn events(&self) -> Vec<(&'static str, &'static str)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl PrecompileCallObserver for RecordingObserver {
        fn start(&self, label: &'static str) {
            self.events.lock().unwrap().push(("start", label));
        }

        fn end(&self, label: &'static str) {
            self.events.lock().unwrap().push(("end", label));
        }
    }

    #[test]
    fn observe_brackets_result() {
        let observer = RecordingObserver::new();
        let result = observer.observe("precompile-b20-transfer", || 42);

        assert_eq!(result, 42);
        assert_eq!(
            observer.events(),
            [("start", "precompile-b20-transfer"), ("end", "precompile-b20-transfer"),]
        );
    }

    #[test]
    fn observe_ends_when_observed_work_panics() {
        let observer = RecordingObserver::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            observer.observe("precompile-b20-transfer", || panic!("observed panic"));
        }));
        assert!(result.is_err());
        assert_eq!(
            observer.events(),
            [("start", "precompile-b20-transfer"), ("end", "precompile-b20-transfer"),]
        );
    }
}
