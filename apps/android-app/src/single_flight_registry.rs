use anyhow::Result;
use std::sync::{Arc, Condvar, Mutex};

type SharedBuildResult<Value> = std::result::Result<Value, Arc<String>>;

struct InFlightBuild<Key, Value> {
    key: Key,
    result: Mutex<Option<SharedBuildResult<Value>>>,
    completed: Condvar,
}

impl<Key, Value> InFlightBuild<Key, Value>
where
    Value: Clone,
{
    fn new(key: Key) -> Self {
        Self {
            key,
            result: Mutex::new(None),
            completed: Condvar::new(),
        }
    }

    fn complete(&self, result: SharedBuildResult<Value>) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(result);
        self.completed.notify_all();
    }

    fn wait(&self) -> SharedBuildResult<Value> {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(result) = slot.as_ref() {
                return result.clone();
            }
            slot = self
                .completed
                .wait(slot)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

struct ReadyValue<Key, Value> {
    key: Key,
    value: Value,
}

struct RegistryState<Key, Value> {
    ready: Option<ReadyValue<Key, Value>>,
    in_flight: Option<Arc<InFlightBuild<Key, Value>>>,
}

impl<Key, Value> Default for RegistryState<Key, Value> {
    fn default() -> Self {
        Self {
            ready: None,
            in_flight: None,
        }
    }
}

/// Keeps one ready value and makes concurrent callers for the same key join a
/// single construction attempt, including its failure result.
pub(crate) struct SingleFlightRegistry<Key, Value> {
    state: Mutex<RegistryState<Key, Value>>,
}

impl<Key, Value> Default for SingleFlightRegistry<Key, Value> {
    fn default() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
        }
    }
}

impl<Key, Value> SingleFlightRegistry<Key, Value>
where
    Key: Clone + Eq,
    Value: Clone,
{
    pub(crate) fn get_or_try_init(
        &self,
        key: Key,
        build: impl FnOnce(&Key) -> Result<Value>,
    ) -> Result<Value> {
        let mut build = Some(build);
        loop {
            match self.next_action(&key) {
                RegistryAction::Ready(value) => return Ok(value),
                RegistryAction::WaitForSameKey(in_flight) => {
                    return shared_result(in_flight.wait());
                }
                RegistryAction::WaitForOtherKey(in_flight) => {
                    let _ = in_flight.wait();
                }
                RegistryAction::Build(in_flight) => {
                    let builder = build
                        .take()
                        .expect("single-flight build closure can only run once");
                    let result = catch_build_panic(|| builder(&key));
                    in_flight.complete(result.clone());
                    self.finish_build(key.clone(), &in_flight, &result);
                    return shared_result(result);
                }
            }
        }
    }

    fn next_action(&self, key: &Key) -> RegistryAction<Key, Value> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(ready) = state.ready.as_ref()
            && ready.key == *key
        {
            return RegistryAction::Ready(ready.value.clone());
        }

        match state.in_flight.as_ref() {
            Some(in_flight) if in_flight.key == *key => {
                RegistryAction::WaitForSameKey(in_flight.clone())
            }
            Some(in_flight) => RegistryAction::WaitForOtherKey(in_flight.clone()),
            None => {
                let in_flight = Arc::new(InFlightBuild::new(key.clone()));
                state.in_flight = Some(in_flight.clone());
                RegistryAction::Build(in_flight)
            }
        }
    }

    fn finish_build(
        &self,
        key: Key,
        in_flight: &Arc<InFlightBuild<Key, Value>>,
        result: &SharedBuildResult<Value>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Ok(value) = result {
            state.ready = Some(ReadyValue {
                key,
                value: value.clone(),
            });
        }
        if state
            .in_flight
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, in_flight))
        {
            state.in_flight = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = RegistryState::default();
    }
}

enum RegistryAction<Key, Value> {
    Ready(Value),
    Build(Arc<InFlightBuild<Key, Value>>),
    WaitForSameKey(Arc<InFlightBuild<Key, Value>>),
    WaitForOtherKey(Arc<InFlightBuild<Key, Value>>),
}

fn catch_build_panic<Value>(build: impl FnOnce() -> Result<Value>) -> SharedBuildResult<Value> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
        Ok(result) => result.map_err(|error| Arc::new(format!("{error:#}"))),
        Err(_) => Err(Arc::new("single-flight construction panicked".to_string())),
    }
}

fn shared_result<Value>(result: SharedBuildResult<Value>) -> Result<Value> {
    result.map_err(|error| anyhow::anyhow!(error.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar};
    use std::thread;

    #[test]
    fn concurrent_failed_callers_share_one_build_generation() {
        const CALLER_COUNT: usize = 6;

        let registry = Arc::new(SingleFlightRegistry::<String, usize>::default());
        let build_count = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(CALLER_COUNT + 1));
        let release = Arc::new((Mutex::new(false), Condvar::new()));

        let callers = (0..CALLER_COUNT)
            .map(|_| {
                let registry = registry.clone();
                let build_count = build_count.clone();
                let start = start.clone();
                let release = release.clone();
                thread::spawn(move || {
                    start.wait();
                    registry
                        .get_or_try_init("active-client".to_string(), |_| {
                            build_count.fetch_add(1, Ordering::SeqCst);
                            let (lock, completed) = &*release;
                            let mut can_finish =
                                lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            while !*can_finish {
                                can_finish = completed
                                    .wait(can_finish)
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                            }
                            anyhow::bail!("shared build failure")
                        })
                        .expect_err("the shared build generation should fail")
                        .to_string()
                })
            })
            .collect::<Vec<_>>();

        start.wait();
        let expected_references = CALLER_COUNT + 1;
        for _ in 0..10_000 {
            let references = registry
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .in_flight
                .as_ref()
                .map(Arc::strong_count)
                .unwrap_or_default();
            if references >= expected_references {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
        assert!(
            registry
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .in_flight
                .as_ref()
                .is_some_and(|flight| Arc::strong_count(flight) >= expected_references),
            "every concurrent caller should join the same build generation",
        );

        let (lock, completed) = &*release;
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        completed.notify_all();

        let errors = callers
            .into_iter()
            .map(|caller| caller.join().expect("client caller should not panic"))
            .collect::<Vec<_>>();
        assert!(errors.iter().all(|error| error == "shared build failure"));
        assert_eq!(build_count.load(Ordering::SeqCst), 1);

        let retry = registry
            .get_or_try_init("active-client".to_string(), |_| {
                build_count.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("later retry failure")
            })
            .expect_err("a later caller may start a new build generation");
        assert_eq!(retry.to_string(), "later retry failure");
        assert_eq!(build_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_panicking_builder_does_not_leave_the_registry_stuck() {
        let registry = SingleFlightRegistry::<String, usize>::default();

        let panic = registry
            .get_or_try_init("active-client".to_string(), |_| panic!("boom"))
            .expect_err("panic should become a shared construction error");
        assert_eq!(panic.to_string(), "single-flight construction panicked");

        assert_eq!(
            registry
                .get_or_try_init("active-client".to_string(), |_| Ok(42))
                .expect("a later generation should still build"),
            42,
        );
    }
}
