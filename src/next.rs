use std::{
    time::{Instant, Duration},
    collections::VecDeque,
    marker::PhantomData,
    sync::{Mutex, Condvar, Arc, atomic::AtomicUsize}, fmt, error,
};

// TODO: shut down debouncer on drop

/// Creates a new debouncer for deduplicating groups of "raw" events which occur at a similar time.
/// The debouncer is comprised of two halves; a [`DebouncerTx`](DebouncerTx) for sending raw events
/// to the debouncer, and a [`DebouncerRx`](DebouncerRx) for receiving grouped (debounced) events
/// from the debouncer.
/// 
/// When a raw event is sent through the `DebouncerTx`, future `debounce_events` which are sent
/// before the given `debounce_time` has elapsed are considered part of the same group. For
/// example, if the `debounce_time` is 1 second, all raw events which are sent less than one second
/// after the first are considered part of the same group.
/// 
/// The `DebouncerRx` receives the grouped events from the debouncer once the `debounce_time` has
/// elapsed for the event. Events are grouped using the given `fold` function, which takes a
/// previous grouped event (or "accumulator") and combines it with a new raw event.
/// 
/// ```
/// # use std::{thread, time::Duration};
/// # use treacle::next::debouncer;
/// // Create a new debouncer which takes raw events of type `u32` and combines
/// // them by pushing them to a vector.
/// let (tx, rx) = debouncer::<u32, Vec<u32>, _>(
///     Duration::from_millis(500),
///     |acc, raw_event| {
///         let mut events_vector = acc.unwrap_or_default();
///         events_vector.push(raw_event);
///         events_vector
///     });
/// 
/// thread::spawn(move || {
///     // Send two raw events in quick succession.
///     tx.send(10).unwrap();
///     tx.send(20).unwrap();
/// 
///     // Wait, then send another raw event.
///     thread::sleep(Duration::from_millis(500));
///     tx.send(30).unwrap();
/// });
/// 
/// assert_eq!(rx.recv().unwrap(), &[10, 20]);
/// assert_eq!(rx.recv().unwrap(), &[30]);
/// ```
pub fn debouncer<R, D, F>(debounce_time: Duration, fold: F)
    -> (DebouncerTx<R, D, F>, DebouncerRx<D>)
where
    F: Fn(Option<D>, R) -> D,
{
    let shared_state = Arc::new(Debouncer {
        state: Mutex::new(DebouncerState::new()),
        debounce_time,
        queue_wait_cvar: Condvar::new(),
        event_ready_wait_cvar: Condvar::new(),
        tx_count: AtomicUsize::new(1),
        rx_count: AtomicUsize::new(1),
    });
    
    let tx = DebouncerTx {
        debouncer: shared_state.clone(),
        fold,
        _raw_event_marker: PhantomData,
    };

    let rx = DebouncerRx {
        debouncer: shared_state,
    };

    (tx, rx)
}

pub struct DebouncerTx<R, D, F> {
    debouncer: Arc<Debouncer<D>>,
    fold: F,
    _raw_event_marker: PhantomData<R>,
}

impl<R, D, F> DebouncerTx<R, D, F>
where
    F: Fn(Option<D>, R) -> D,
{
    pub fn send(&self, event: R) -> Result<(), SendError<R>> {
        self.debouncer.push(event, &self.fold)
    }
}

// FIXME: increment some sort of tx count
impl<R, D, F> Clone for DebouncerTx<R, D, F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            debouncer: self.debouncer.clone(),
            fold: self.fold.clone(),
            _raw_event_marker: PhantomData,
        }
    }
}

// FIXME: check tx count and shut down if zero
impl<R, D, F> Drop for DebouncerTx<R, D, F> {
    fn drop(&mut self) {
        todo!()
    }
}

pub struct DebouncerRx<D> {
    debouncer: Arc<Debouncer<D>>
}

impl<D> DebouncerRx<D> {
    pub fn recv(&self) -> Result<D, ReceiveError> {
        self.debouncer.pop()
    }

    pub fn try_recv(&self) -> Result<Option<D>, ReceiveError> {
        self.debouncer.try_pop()
    }
}

// FIXME: increment some sort of rx count
impl<D> Clone for DebouncerRx<D> {
    fn clone(&self) -> Self {
        Self {
            debouncer: self.debouncer.clone(),
        }
    }
}

// FIXME: check rx count and shut down if zero
impl<D> Drop for DebouncerRx<D> {
    fn drop(&mut self) {
        todo!()
    }
}

pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendError").finish_non_exhaustive()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt("sending through a closed debouncer", f)
    }
}

impl<T> error::Error for SendError<T> {}

#[derive(Debug)]
pub struct ReceiveError;

impl fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt("receiving from a closed debouncer", f)
    }
}

impl error::Error for ReceiveError {}

struct Debouncer<T> {
    state: Mutex<DebouncerState<T>>,
    debounce_time: Duration,
    queue_wait_cvar: Condvar,
    event_ready_wait_cvar: Condvar,
    tx_count: AtomicUsize,
    rx_count: AtomicUsize,
}

impl<T> Debouncer<T> {
    fn push<R, F>(&self, raw_event: R, fold: F) -> Result<(), SendError<R>>
    where
        F: Fn(Option<T>, R) -> T,
    {
        let now = Instant::now();

        let push_outcome = {
            let mut state_guard = self.state.lock().unwrap();
            
            // Return an error if the debouncer is closed. Include the raw event in the error so
            // that it isn't lost.
            if state_guard.shutdown {
                return Err(SendError(raw_event));
            }

            state_guard.acc_queue.push_latest(raw_event, fold, now, self.debounce_time)
        };
        
        if matches!(push_outcome, PushOutcome::NewAcc) {
            // If we pushed a new event accumulator, wake up one rx thread which was waiting for
            // the queue to be non-empty. Don't wake up all the rx threads because only one of
            // them can consume the acc from the queue; therefore, we should wait until another
            // acc is added to the queue before waking up another thread.
            self.queue_wait_cvar.notify_one();
        }

        Ok(())
    }

    fn pop(&self) -> Result<T, ReceiveError> {
        enum PopOutcome<T> {
            Event(T),
            Shutdown,
        }

        let mut state_guard = self.state.lock().unwrap();

        let result = 'result: {
            if state_guard.shutdown {
                break 'result PopOutcome::Shutdown;
            }

            // Loop because we may have a situation where:
            // 1. We wait for the queue to be non-empty.
            // 2. The accumulator in the queue, `x`, has a `ready_time` in the future, so we must
            //    wait for it to be ready.
            // 3. Before we can wake up from waiting, some other thread pops the now-ready `x`, and
            //    the queue is now empty.
            // 4. We wake up and must wait for the queue to become non-empty again.  
            'pop_event_outer_loop: loop {
                // If there are no accumulators in the queue, wait for one to be pushed.
                if state_guard.acc_queue.is_empty() {
                    // Park the thread. We will be woken up again either by a new accumulator being
                    // pushed to the queue, or by the debouncer being shut down.
                    state_guard = self.queue_wait_cvar.wait(state_guard).unwrap();
    
                    // We may have been unparked because someone wants to shut down the debouncer,
                    // so check the shutdown flag.
                    if state_guard.shutdown {
                        break PopOutcome::Shutdown;
                    }
                }

                // Loop because we may have a situation where:
                // 1. The first accumulator in the queue, `x`, has a `ready_time` in the future.
                // 2. We begin waiting for `x` to become ready.
                // 3. Before we can wake up from waiting, some other thread pops the now-ready `x`.
                // 4. The new first accumulator in the queue, `y`, has a `ready_time` in the queue.
                // 5. We wake up and must start waiting again, as `y` is not ready yet.
                break loop {
                    let required_wait_time = {
                        // Get the oldest accumulator in the queue, so we can see if it is ready to
                        // be popped and, if not, how long we need to wait for it to be ready to be
                        // popped.
                        let Some(peeked_acc) = state_guard.acc_queue.peek_oldest() else {
                            // If there is no accumulator for us to pop from the queue, go back to
                            // waiting for the queue to be non-empty.
                            continue 'pop_event_outer_loop;
                        };
        
                        let now = Instant::now();

                        // Calculate the time we need to wait for the accumulator to become ready,
                        // if at all.
                        peeked_acc.ready_time.checked_duration_since(now)
                    };

                    match required_wait_time {
                        // Case where we must wait for the accumulator to become ready.
                        Some(wait_time) if !wait_time.is_zero() => {
                            // Wait the amount of time between now and the `ready_time` of the
                            // accumulator. This is done using a condvar so the sleep can be
                            // interrupted if someone wants to shut down the debouncer.
                            (state_guard, _) = self.event_ready_wait_cvar
                                .wait_timeout(state_guard, wait_time)
                                .unwrap();
    
                            // Again, we may have been unparked because someone wants to shut down the
                            // debouncer, so check the shutdown flag and return if it is set.
                            if state_guard.shutdown {
                                break PopOutcome::Shutdown;
                            }
        
                            continue;
                        },

                        // If we don't have to wait, we can pop the oldest accumulator from the
                        // queue and return it. We can unwrap the `pop_oldest` because in this
                        // case, we successfully peeked the queue and haven't modified the queue or
                        // released the lock since.
                        _ => {
                            match state_guard.acc_queue.pop_oldest().unwrap().into_acc() {
                                Some(popped_acc) => break PopOutcome::Event(popped_acc),

                                // If the accumulator we popped has no data (which can happen if
                                // the fold function panicked while pushing), retry from the
                                // beginning.
                                None => continue 'pop_event_outer_loop,
                            }
                        },
                    }
                };
            }
        };

        match result {
            PopOutcome::Event(event) => Ok(event),

            // If the debouncer has been shut down, return any remaining accumulators then start
            // returning errors once the accumulators have been depleted.
            PopOutcome::Shutdown => {
                state_guard
                    .acc_queue
                    .pop_oldest_acc_discard_none()
                    .ok_or(ReceiveError)
            },
        }
    }

    fn try_pop(&self) -> Result<Option<T>, ReceiveError> {
        let mut state_guard = self.state.lock().unwrap();

        // If the debouncer has been shut down, return any remaining accumulators then start
        // returning errors once the accumulators have been depleted.
        if state_guard.shutdown {
            return state_guard
                .acc_queue
                .pop_oldest_acc_discard_none()
                .map(Some)
                .ok_or(ReceiveError);
        }

        let now = Instant::now();

        // Loop in case we pop an accumulator from the queue which contains no data, which can
        // happen if the fold function panicked when pushing.
        loop {
            let first_acc_ready = {
                let Some(peeked_acc) = state_guard.acc_queue.peek_oldest() else {
                    break Ok(None);
                };

                peeked_acc.ready_time <= now
            };

            if first_acc_ready {
                match state_guard.acc_queue.pop_oldest().unwrap().into_acc() {
                    Some(popped_acc) => break Ok(Some(popped_acc)),
                    None => continue,
                }
            } else {
                break Ok(None);
            }
        }
    }
}

struct DebouncerState<T> {
    acc_queue: EventAccQueue<T>,
    shutdown: bool,
}

impl<T> DebouncerState<T> {
    fn new() -> Self {
        Self {
            acc_queue: EventAccQueue::new(),
            shutdown: false,
        }
    }
}

struct EventAccQueue<T> {
    inner: VecDeque<EventAcc<T>>,
}

impl<T> EventAccQueue<T> {
    fn new() -> Self {
        Self { inner: VecDeque::new() }
    }
    
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn peek_oldest(&self) -> Option<&EventAcc<T>> {
        self.inner.front()
    }

    fn pop_oldest(&mut self) -> Option<EventAcc<T>> {
        self.inner.pop_front()
    }

    fn pop_oldest_acc_discard_none(&mut self) -> Option<T> {
        while let Some(event) = self.pop_oldest() {
            if let Some(acc) = event.into_acc() {
                return Some(acc);
            }
        }
        return None;
    }

    fn push_latest<R, F>(&mut self, raw_event: R, f: F, now: Instant, debounce_time: Duration)
        -> PushOutcome
    where
        F: FnOnce(Option<T>, R) -> T,
    {
        // Get the last accumulator in the queue if and only if the queue is non-empty and the last
        // accumulator in the queue is ready (based on the given `now` time).
        match self.inner
            .back_mut()
            .and_then(|acc| (acc.ready_time > now).then_some(acc))
        {
            // If the last accumulator in the queue is not ready yet, we can fold the new event
            // into the accumulator.
            Some(event) => {
                event.fold(raw_event, f);
                PushOutcome::NoNewAcc
            },

            // If the last accumulator in the queue is ready, we don't want to keep adding to it,
            // so make a new accumulator and push it to the end of the queue.
            None => {
                let ready_time = now + debounce_time;
                let event = EventAcc::new_from_fold(raw_event, f, ready_time);
                self.inner.push_back(event);
                PushOutcome::NewAcc
            },
        }
    }
}

enum PushOutcome {
    NewAcc,
    NoNewAcc,
}

/// "Event accumulator", created by repeatedly joining raw events using a fold function.
struct EventAcc<T> {
    // The accumulator is stored as an `Option` because it is temporarily set to `None` during
    // `EventAcc::fold`, so that the old accumulator can be moved into the user-provided fold
    // function rather than borrowing it. `MaybeUninit` cannot be used for this because the
    // user-provided fold function may panic, which would leave the accumulator in an uninitialised
    // state, potentially causing UB later.
    acc: Option<T>,
    ready_time: Instant,
}

impl<T> EventAcc<T> {
    fn new_from_fold<R, F>(raw_event: R, f: F, ready_time: Instant) -> Self
    where
        F: FnOnce(Option<T>, R) -> T,
    {
        Self {
            acc: Some(f(None, raw_event)),
            ready_time,
        }
    }

    fn fold<R, F>(&mut self, raw_event: R, f: F)
    where
        F: FnOnce(Option<T>, R) -> T,
    {
        let acc = self.acc.take();
        self.acc = Some(f(acc, raw_event));
    }

    fn into_acc(self) -> Option<T> {
        self.acc
    }
}

