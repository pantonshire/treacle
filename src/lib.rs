//! A generic event debouncer, for grouping events which occur close together in time into a single
//! event.
//! 
//! For example, you may have some
//! [templates](https://www.arewewebyet.org/topics/templating/)
//! which you want to reload whenever a template file changes. However, if many small changes are
//! made to the template files in quick succession, it would be wasteful to reload the templates
//! for every change; instead, a debouncer could be used to group the changes that occur at a
//! similar time into a single change, so the templates are only reloaded once.
//! 
//! A new debouncer can be created with the [`debouncer`](debouncer) function, which returns the
//! debouncer in two halves: a tx (send) half and rx (receive) half. The tx sends raw un-debounced
//! events to the debouncer, and the rx receives the debounced events from the debouncer. Both
//! halves can be cloned to allow for multiple senders and receivers.
//! 
//! ```
//! # use std::{thread, time::Duration};
//! // Create a new debouncer which takes raw events of type `u32` and outputs
//! // debounced events of type `Vec<u32>`.
//! let (tx, rx) = treacle::debouncer::<u32, Vec<u32>, _>(
//!     // Group events which occur in the same 500ms window.
//!     Duration::from_millis(500),
//!     // Combine raw events by pushing them to a vector.
//!     |acc, raw_event| {
//!         let mut events_vector = acc.unwrap_or_default();
//!         events_vector.push(raw_event);
//!         events_vector
//!     });
//! 
//! thread::spawn(move || {
//!     // Send two raw events in quick succession.
//!     tx.send(10).unwrap();
//!     tx.send(20).unwrap();
//! 
//!     // Wait, then send more raw events.
//!     thread::sleep(Duration::from_millis(500));
//!     tx.send(30).unwrap();
//!     tx.send(40).unwrap();
//!     tx.send(50).unwrap();
//! });
//! 
//! assert_eq!(rx.recv().unwrap(), &[10, 20]);
//! assert_eq!(rx.recv().unwrap(), &[30, 40, 50]);
//! ```

pub mod fold;

use std::{
    time::{Instant, Duration},
    collections::VecDeque,
    marker::PhantomData,
    sync::{Mutex, Condvar, Arc, MutexGuard},
    fmt,
    error,
};

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
/// previous grouped event (or "accumulator") and combines it with a new raw event. Some
/// pre-written fold functions are available in the [`fold`](fold) module.
/// 
/// ```
/// # use std::{thread, time::Duration};
/// // Create a new debouncer which takes raw events of type `u32` and combines
/// // them by pushing them to a vector.
/// let (tx, rx) = treacle::debouncer::<u32, Vec<u32>, _>(
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
/// 
/// Both the `DebouncerTx` and `DebouncerRx` may be cloned, allowing multiple event senders and
/// multiple event receivers for a single debouncer.
pub fn debouncer<R, D, F>(debounce_time: Duration, fold: F)
    -> (DebouncerTx<R, D, F>, DebouncerRx<D>)
where
    F: Fn(Option<D>, R) -> D,
{
    let shared_state = Arc::new(Debouncer::new(debounce_time, 1, 1));
    
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

/// The "send" half of a debouncer, through which raw events are sent to be debounced. To create a
/// new debouncer, see the [`debouncer`](debouncer) function. To send raw events, see
/// [`DebouncerTx::send`](DebouncerTx::send).
pub struct DebouncerTx<R, D, F> {
    debouncer: Arc<Debouncer<D>>,
    fold: F,
    _raw_event_marker: PhantomData<R>,
}

impl<R, D, F> DebouncerTx<R, D, F>
where
    F: Fn(Option<D>, R) -> D,
{
    /// Send a raw event to the debouncer, which will be grouped with other raw events sent at a
    /// similar time. Grouping is performed using the "fold" function this `DebouncerTx` was
    /// created with.
    /// 
    /// An error is returned if there are no [`DebouncerRx`](DebouncerRx)s associated with the
    /// debouncer left to receive events.
    /// 
    /// ```
    /// # use std::{panic, thread, time::Duration};
    /// // Create a new debouncer which takes raw events of type `u32` and
    /// // combines them by converting them to `u64`s and adding them together.
    /// let (tx, rx) = treacle::debouncer::<u32, u64, _>(
    ///     Duration::from_millis(500),
    ///     |acc, raw_event| {
    ///         acc.unwrap_or_default() + u64::from(raw_event)
    ///     });
    /// 
    /// // Spawn a thread to receive the events we are about to send.
    /// let t = thread::spawn(move || {
    ///     assert_eq!(rx.recv().unwrap(), 3);  // 1 + 2
    ///     assert_eq!(rx.recv().unwrap(), 12); // 3 + 4 + 5
    /// });
    /// 
    /// // Send two raw events in quick succession.
    /// tx.send(1).unwrap();
    /// tx.send(2).unwrap();
    /// 
    /// // Wait, then send three more raw events.
    /// thread::sleep(Duration::from_millis(500));
    /// tx.send(3).unwrap();
    /// tx.send(4).unwrap();
    /// tx.send(5).unwrap();
    /// 
    /// // Wait for the receive thread to finish.
    /// if let Err(err) = t.join() {
    ///     panic::resume_unwind(err);
    /// }
    /// 
    /// // We moved `rx` into the receive thread, so when the receive thread
    /// // finishes, `rx` is dropped. `rx` was the only `DebouncerRx`, so there
    /// // is nothing left to receive events we send, so an error is returned.
    /// assert!(tx.send(6).is_err());
    /// ```
    pub fn send(&self, event: R) -> Result<(), SendError<R>> {
        self.debouncer.push(event, &self.fold)
    }
}

impl<R, D, F> Clone for DebouncerTx<R, D, F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        self.debouncer
            .lock_state()
            .add_tx()
            .expect("debouncer tx count should not overflow");
        
        Self {
            debouncer: self.debouncer.clone(),
            fold: self.fold.clone(),
            _raw_event_marker: PhantomData,
        }
    }
}

impl<R, D, F> Drop for DebouncerTx<R, D, F> {
    fn drop(&mut self) {
        let remaining_tx = {
            let mut state_guard = self.debouncer.lock_state();
            state_guard.remove_tx()
        };

        if remaining_tx == 0 {
            // There may be some rx threads waiting on the condvars, so notify them to stop them
            // from waiting. Upon waking up, the rx threads will see that the tx count is 0 and
            // switch to their "shutdown" behaviour.
            self.debouncer.event_ready_wait_cvar.notify_all();
            self.debouncer.queue_wait_cvar.notify_all();
        }
    }
}

/// The "receive" half of a debouncer, which receives events which have been debounced by grouping
/// raw events which occured close together in time. To create a new debouncer, see the
/// [`debouncer`](debouncer) function. To receive debounced events, see
/// [`DebouncerRx::recv`](DebouncerRx::recv) or [`DebouncerRx::try_recv`](DebouncerRx::try_recv).
pub struct DebouncerRx<D> {
    debouncer: Arc<Debouncer<D>>
}

impl<D> DebouncerRx<D> {
    /// Receive a debounced event from the debouncer. If there is not a debounced event available
    /// to be received, the function will wait for one.
    /// 
    /// An error is returned if there are no more [`DebouncerTx`](DebouncerTx)s left to send events
    /// and there are no more events left to receive.
    /// 
    /// ```
    /// # use std::{panic, thread, time::Duration};
    /// // Create a new debouncer which takes raw events of type `u8` and
    /// // combines them by ORing them together.
    /// let (tx, rx) = treacle::debouncer::<u8, u8, _>(
    ///     Duration::from_millis(500),
    ///     |acc, raw_event| {
    ///         acc.unwrap_or_default() | raw_event
    ///     });
    /// 
    /// // Create a thread to send raw events to the debouncer.
    /// let t = thread::spawn(move || {
    ///     // Send three raw events in quick succession.
    ///     tx.send(0b00001000).unwrap();
    ///     tx.send(0b00000100).unwrap();
    ///     tx.send(0b00000010).unwrap();
    /// 
    ///     // Wait, then send more raw events.
    ///     thread::sleep(Duration::from_millis(500));
    ///     tx.send(0b10000000).unwrap();
    ///     tx.send(0b00100000).unwrap();
    /// 
    ///     // Wait, then send a final batch of raw events.
    ///     thread::sleep(Duration::from_millis(500));
    ///     tx.send(0b01000000).unwrap();
    ///     tx.send(0b00000001).unwrap();
    /// });
    /// 
    /// // Receive the first two debounced events from the debouncer, created
    /// // by ORing the raw events together that occurred within the same 500ms
    /// // window.
    /// assert_eq!(rx.recv().unwrap(), 0b00001110);
    /// assert_eq!(rx.recv().unwrap(), 0b10100000);
    /// 
    /// // Wait for the send thread to finish. The send thread owns `tx`, so it
    /// // will be dropped once the thread finishes.
    /// if let Err(err) = t.join() {
    ///     panic::resume_unwind(err);
    /// }
    /// 
    /// // `tx` has been dropped, but there is still one last event left for us
    /// // to receive.
    /// assert_eq!(rx.recv().unwrap(), 0b01000001);
    /// 
    /// // There are no more events to receive and no more `DebouncerTx`s left
    /// // to send events, so attempting to receive results in an error.
    /// assert!(rx.recv().is_err());
    /// ```
    pub fn recv(&self) -> Result<D, ReceiveError> {
        self.debouncer.pop()
    }

    /// An alternative to [`DebouncerRx::recv`](DebouncerRx::recv) which returns `None` if there is
    /// not a debounced event immediately available, instead of waiting for it to become available.
    /// 
    /// An error is returned if there are no more [`DebouncerTx`](DebouncerTx)s left to send events
    /// and there are no more events left to receive.
    /// 
    /// ```
    /// # use std::{thread, time::Duration};
    /// // Create a new debouncer which takes raw events of type `u8` and
    /// // combines them by ORing them together.
    /// let (tx, rx) = treacle::debouncer::<u8, u8, _>(
    ///     Duration::from_millis(500),
    ///     |acc, raw_event| {
    ///         acc.unwrap_or_default() | raw_event
    ///     });
    /// 
    /// tx.send(0b00001000).unwrap();
    /// tx.send(0b00000100).unwrap();
    /// tx.send(0b00000010).unwrap();
    /// 
    /// assert_eq!(rx.try_recv().unwrap(), None);
    /// 
    /// thread::sleep(Duration::from_millis(500));
    /// assert_eq!(rx.try_recv().unwrap(), Some(0b00001110));
    /// ```
    pub fn try_recv(&self) -> Result<Option<D>, ReceiveError> {
        self.debouncer.try_pop()
    }
}

impl<D> Clone for DebouncerRx<D> {
    fn clone(&self) -> Self {
        self.debouncer
            .lock_state()
            .add_rx()
            .expect("debouncer rx count should not overflow");

        Self {
            debouncer: self.debouncer.clone(),
        }
    }
}

impl<D> Drop for DebouncerRx<D> {
    fn drop(&mut self) {
        // Decrement the rx count. We don't need to notify any threads waiting on condvars if the
        // count reaches 0, because only rx threads wait on the condvars and we know there are no
        // more rx threads (because the count just reached 0!)
        self.debouncer.lock_state().remove_rx();
    }
}

/// An error indicating that there are no more [`DebouncerRx`](DebouncerRx)s left to receive
/// events. The raw event that could not be sent is included in the error so it is not lost.
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

/// An error indicating that the debouncer has no more events left to receive, and there are no
/// more [`DebouncerTx`](DebouncerTx)s left to send events.
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
}

impl<T> Debouncer<T> {
    fn new(debounce_time: Duration, tx_count: usize, rx_count: usize) -> Self {
        Self {
            state: Mutex::new(DebouncerState::new(tx_count, rx_count)),
            debounce_time,
            queue_wait_cvar: Condvar::new(),
            event_ready_wait_cvar: Condvar::new(),
        }
    }

    fn lock_state(&self) -> MutexGuard<DebouncerState<T>> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        }
    }
    
    fn push<R, F>(&self, raw_event: R, fold: F) -> Result<(), SendError<R>>
    where
        F: Fn(Option<T>, R) -> T,
    {
        let now = Instant::now();

        let push_outcome = {
            let mut state_guard = self.lock_state();
            
            // Return an error if there are no rxs left to send to. Include the raw event in the
            // error so that it isn't lost.
            if state_guard.has_no_rxs() {
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

        let mut state_guard = self.lock_state();

        let result = 'result: {
            // Check that there are any txs left so we don't get stuck waiting forever if the queue
            // is empty (as a tx is the only thing that can wake us up).
            if state_guard.has_no_txs() {
                break 'result PopOutcome::Shutdown;
            }

            // Loop because we may have a situation where:
            // 1. We wait for the queue to be non-empty.
            // 2. The accumulator in the queue, `x`, has a `ready_time` in the future, so we must
            //    wait for it to be ready.
            // 3. Before we can wake up from waiting, some other thread pops the now-ready `x`, and
            //    the queue is now empty.
            // 4. We wake up and must wait for the queue to become non-empty again.
            //
            // Additionally, the loop also handles spurious returns from the condvar wait.
            'pop_event_outer_loop: loop {
                // If there are no accumulators in the queue, wait for one to be pushed.
                if state_guard.acc_queue.is_empty() {
                    // Park the thread. We will be woken up again either by a new accumulator being
                    // pushed to the queue, or by the debouncer being shut down.
                    // 
                    // This may return spuriously so the queue may still be empty when we wake up,
                    // but if this happens we will retry the wait when we attempt and fail to peek
                    // the queue later.
                    state_guard = self.queue_wait_cvar.wait(state_guard).unwrap();
    
                    // We may have been unparked because someone wants to shut down the debouncer,
                    // so check the shutdown flag.
                    if state_guard.has_no_txs() {
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
                            // waiting for the queue to be non-empty. This could happen because the
                            // previous condvar wait returned spuriously, or because another thread
                            // popped the accumulator before us.
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
                            //
                            // This may return spuriously so we may wake up before the time has
                            // elapsed and without being notified by anyone. However, since we go
                            // back so the start of the loop after this, we will see that there is
                            // still time remaining before the accumulator becomes ready, and
                            // therefore we will resume waiting.
                            (state_guard, _) = self.event_ready_wait_cvar
                                .wait_timeout(state_guard, wait_time)
                                .unwrap();
    
                            // Again, we may have been unparked because someone wants to shut down the
                            // debouncer, so check the shutdown flag and return if it is set.
                            if state_guard.has_no_txs() {
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
        let mut state_guard = self.lock_state();

        // If the debouncer has been shut down, return any remaining accumulators then start
        // returning errors once the accumulators have been depleted.
        if state_guard.has_no_txs() {
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
    // These could alternatively be `AtomicUsize`s which live outside the mutex, but we opt to have
    // them inside the mutex instead to avoid a confusing mixture of mutexes / condvars and
    // atomics.
    tx_count: usize,
    rx_count: usize,
}

impl<T> DebouncerState<T> {
    fn new(tx_count: usize, rx_count: usize) -> Self {
        Self {
            acc_queue: EventAccQueue::new(),
            tx_count,
            rx_count,
        }
    }

    fn has_no_txs(&self) -> bool {
        self.tx_count == 0
    }

    fn add_tx(&mut self) -> Result<(), CountOverflowError> {
        self.tx_count = self.tx_count.checked_add(1)
            .ok_or(CountOverflowError)?;

        Ok(())
    }

    fn remove_tx(&mut self) -> usize {
        self.tx_count = self.tx_count.saturating_sub(1);
        self.tx_count
    }

    fn has_no_rxs(&self) -> bool {
        self.rx_count == 0
    }

    fn add_rx(&mut self) -> Result<(), CountOverflowError> {
        self.rx_count = self.rx_count.checked_add(1)
            .ok_or(CountOverflowError)?;

        Ok(())
    }

    fn remove_rx(&mut self) -> usize {
        self.rx_count = self.rx_count.saturating_sub(1);
        self.rx_count
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

#[derive(Debug)]
struct CountOverflowError;

#[cfg(test)]
mod tests {
    use std::{time::{Duration, Instant}, thread};

    use super::{debouncer, fold};

    #[test]
    fn test_debounce() {
        let (tx, rx) = debouncer(Duration::from_millis(50), fold::fold_vec_push::<u8>);

        for i in 0..3 {
            for j in 0..10 {
                tx.send(i * 10 + j).unwrap();
                thread::sleep(Duration::from_millis(4));
            }

            thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(rx.recv().unwrap(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(rx.recv().unwrap(), &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
        assert_eq!(rx.recv().unwrap(), &[20, 21, 22, 23, 24, 25, 26, 27, 28, 29]);
    }

    #[test]
    fn test_debouncer_shutdown() {
        let (tx, rx) = debouncer(Duration::from_millis(100), fold::fold_vec_push::<u8>);

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();

        // Drop the tx, shutting down the debouncer.
        drop(tx);

        // Test that the events emitted just before the shutdown are not lost.
        assert_eq!(rx.recv().unwrap(), &[1, 2, 3]);
        assert!(rx.recv().is_err());
    }

    #[test]
    fn test_multi_tx() {
        let (tx1, rx) = debouncer(Duration::from_millis(100), fold::fold_vec_push::<u8>);

        let tx2 = tx1.clone();

        tx1.send(1).unwrap();
        tx2.send(2).unwrap();
        assert_eq!(rx.recv().unwrap(), &[1, 2]);

        drop(tx1);

        tx2.send(3).unwrap();
        tx2.send(4).unwrap();
        assert_eq!(rx.recv().unwrap(), &[3, 4]);

        let start_time = Instant::now();
        tx2.send(5).unwrap();
        tx2.send(6).unwrap();
        assert_eq!(rx.recv().unwrap(), &[5, 6]);
        assert!(Instant::now().duration_since(start_time) >= Duration::from_millis(100));

        tx2.send(7).unwrap();
        tx2.send(8).unwrap();
        drop(tx2);
        assert_eq!(rx.recv().unwrap(), &[7, 8]);
        assert!(rx.recv().is_err());
    }

    #[test]
    fn test_multi_rx() {
        let (tx, rx1) = debouncer(Duration::from_millis(100), fold::fold_vec_push::<u8>);

        let rx2 = rx1.clone();

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx1.recv().unwrap(), &[1, 2]);

        tx.send(3).unwrap();
        tx.send(4).unwrap();
        assert_eq!(rx2.recv().unwrap(), &[3, 4]);

        drop(rx1);

        tx.send(5).unwrap();
        tx.send(6).unwrap();
        assert_eq!(rx2.recv().unwrap(), &[5, 6]);

        drop(rx2);

        assert!(tx.send(7).is_err());
    }
}
