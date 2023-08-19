pub mod fold;

use std::{
    sync::{Arc, Mutex, Condvar, mpsc},
    time::Duration,
    thread::{self, JoinHandle},
    io,
    panic, marker::PhantomData,
};

/// A debouncer for deduplicating groups of events which occur at a similar time. Upon receiving an
/// event, the debouncer waits for a specified duration, during which time any additional events
/// will be considered part of the same group. Once it has finished waiting, it will emit a single
/// event of type `T` via an [mpsc](std::sync::mpsc) channel.
/// 
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use treacle::Debouncer;
/// use std::{time::Duration, thread};
/// 
/// let (debouncer, rx) = Debouncer::<u32, u32, _>::new(
///     Duration::from_millis(100),
///     // Combine raw events into one debounced event by adding them
///     |acc, e| acc.unwrap_or_default() + e
/// )?;
///
/// // Send two events to the debouncer in quick succession
/// debouncer.debounce(4);
/// debouncer.debounce(5);
///
/// // Wait, then send two more events to the debouncer
/// thread::sleep(Duration::from_millis(150));
/// debouncer.debounce(3);
/// debouncer.debounce(2);
/// debouncer.debounce(1);
///
/// assert_eq!(rx.recv().unwrap(), 9); // First debounced event (4 + 5)
/// assert_eq!(rx.recv().unwrap(), 6); // Second debounced event (3 + 2 + 1)
/// # Ok(())
/// # }
/// ```
/// 
/// When dropped, the debouncer will close the mpsc channel associated with it.
pub struct Debouncer<RawEvent, DebouncedEvent, FoldFn> {
    thread: Option<JoinHandle<()>>,
    // This is reference counted because the debouncer thread needs access to the controller, and
    // the debouncer itself hasn't been created yet when the debouncer thread is created so we
    // can't give it an `Arc<Debouncer<T>>`. Besides, the thread probably shouldn't have access to
    // its own join handle!
    controller: Arc<DebouncerController<DebouncedEvent>>,
    fold: FoldFn,
    _raw_maker: PhantomData<RawEvent>,
}

impl<RawEvent, DebouncedEvent, FoldFn> Debouncer<RawEvent, DebouncedEvent, FoldFn>
where
    DebouncedEvent: Send + 'static,
    FoldFn: Fn(Option<DebouncedEvent>, RawEvent) -> DebouncedEvent,
{
    /// Create a new debouncer which deduplicates events it receives within the timeframe given by
    /// `debounce_time`. The raw, un-debounced events can be sent to the debouncer with
    /// [`Debouncer::debounce`](crate::Debouncer::debounce), and the resulting debounced events can
    /// be received using the [`mpsc::Receiver`](std::sync::mpsc::Receiver) returned by this
    /// function.
    /// 
    /// Events are deduplicated using the given `fold` function, which combines the current
    /// deduplicated event (an `Option<DebouncedEvent>`) with a new `RawEvent` to produce a new
    /// `DebouncedEvent`. For example, the fold function could collect raw events into a vector:
    /// 
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # use treacle::Debouncer;
    /// # use std::{thread, time::Duration};
    /// let (debouncer, rx) = Debouncer::<u32, Vec<u32>, _>::new(
    ///     Duration::from_millis(100),
    ///     // Combine raw events by pushing them to a vector
    ///     |acc, raw_event| {
    ///         let mut vec = acc.unwrap_or_default();
    ///         vec.push(raw_event);
    ///         vec
    ///     }
    /// )?;
    /// 
    /// // Send two events to the debouncer in quick succession
    /// debouncer.debounce(1);
    /// debouncer.debounce(2);
    /// // Wait, then send another event
    /// thread::sleep(Duration::from_millis(150));
    /// debouncer.debounce(3);
    /// 
    /// assert_eq!(rx.recv().unwrap(), vec![1, 2]);
    /// assert_eq!(rx.recv().unwrap(), vec![3]);
    /// # Ok(())
    /// # }
    /// ```
    /// 
    /// An [`io::Error`](std::io::Error) will be returned by this function if spawning the
    /// debouncer thread fails.
    pub fn new(debounce_time: Duration, fold: FoldFn)
        -> Result<(Self, mpsc::Receiver<DebouncedEvent>), io::Error>
    {
        let controller = Arc::new(DebouncerController {
            state: Mutex::new(DebouncerState::new()),
            main_cvar: Condvar::new(),
            sleep_cvar: Condvar::new(),
        });
        
        let (tx, rx) = mpsc::channel::<DebouncedEvent>();

        let thread = thread::Builder::new().spawn({
            let debounce_state = controller.clone();
            move || debounce_thread(debounce_state, tx, debounce_time)
        })?;
        
        Ok((Self {
            thread: Some(thread),
            controller,
            fold,
            _raw_maker: PhantomData,
        }, rx))
    }
}

impl<RawEvent, DebouncedEvent, FoldFn> Debouncer<RawEvent, DebouncedEvent, FoldFn>
where
    FoldFn: Fn(Option<DebouncedEvent>, RawEvent) -> DebouncedEvent,
{
    /// Send the debouncer a raw event that should be debounced. The
    /// [`mpsc::Receiver`](mpsc::Receiver) associated with this debouncer will receive a message
    /// after at least the amount of time specified when the debouncer was created.
    /// 
    /// The debouncer collects event data into an element of type `T`; the provided function `fold`
    /// specifies how to combine the new event data with the existing event data `Option<T>` to
    /// produce a new `T`. For example, if `T = Vec<E>`, then `fold` could be a function which
    /// pushes to the vec:
    /// 
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # use treacle::Debouncer;
    /// # use std::{thread, time::Duration};
    /// let (debouncer, rx) = Debouncer::<u32, Vec<u32>, _>::new(
    ///     Duration::from_millis(100),
    ///     // Combine raw events by pushing them to a vector
    ///     |acc, raw_event| {
    ///         let mut vec = acc.unwrap_or_default();
    ///         vec.push(raw_event);
    ///         vec
    ///     }
    /// )?;
    /// 
    /// // Send two events to the debouncer in quick succession
    /// debouncer.debounce(1);
    /// debouncer.debounce(2);
    /// // Wait, then send another event
    /// thread::sleep(Duration::from_millis(150));
    /// debouncer.debounce(3);
    /// 
    /// assert_eq!(rx.recv().unwrap(), vec![1, 2]);
    /// assert_eq!(rx.recv().unwrap(), vec![3]);
    /// # Ok(())
    /// # }
    /// ```
    /// 
    /// `fold` is an argument to this function rather than being stored by the `Debouncer` to avoid
    /// ending up with a `Debouncer` with a type that's unnameable (e.g. if a closure is used) or
    /// having to resort to dynamic dispatch.
    #[inline]
    pub fn debounce(&self, event_data: RawEvent) {
        self.controller.notify_event(event_data, &self.fold)
    }
}

impl<RawEvent, DebouncedEvent, FoldFn> Drop for Debouncer<RawEvent, DebouncedEvent, FoldFn> {
    fn drop(&mut self) {
        self.controller.notify_shutdown();

        let thread = self.thread
            .take()
            .expect("debouncer thread has already been shut down");
        
        match thread.join() {
            Ok(()) => (),
            Err(err) => panic::resume_unwind(err),
        }
    }
}

struct DebouncerController<T> {
    state: Mutex<DebouncerState<T>>,
    /// Condvar for notifying the debouncer that it has become dirty or that it should shutdown.
    main_cvar: Condvar,
    /// Condvar for notifying the debouncer that it should cancel sleeping and shutdown.
    sleep_cvar: Condvar,
}

impl<T> DebouncerController<T> {
    /// Notify the debouncer that an event has occurred that should be debounced.
    /// 
    /// The event data is represented by an element of `E`, and the function `f` specifies how to
    /// fold this event into an accumulator of type `T`.
    /// 
    /// For example, to collect events into a list, the accumulator could be a `Vec<E>` and the
    /// fold function could be:
    /// ```ignore
    /// |acc, e| {
    ///     let mut acc = acc.unwrap_or_default();
    ///     acc.push(e);
    ///     acc
    /// }
    /// ```
    #[inline]
    fn notify_event<E, F>(&self, event_data: E, fold: F)
    where
        F: FnOnce(Option<T>, E) -> T,
    {
        let mut guard = self.state.lock().unwrap();
        guard.fold(event_data, fold);
        drop(guard);
        self.main_cvar.notify_one();
    }

    #[inline]
    fn notify_shutdown(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.set_shutdown();
        drop(guard);
        self.main_cvar.notify_one();
        self.sleep_cvar.notify_one();
    }
}

struct DebouncerState<T> {
    /// The debounced data we have received so far.
    acc: Option<T>,
    /// Whether or not we should shutdown the debouncer thread.
    shutdown: bool,
}

impl<T> DebouncerState<T> {
    fn new() -> Self {
        Self { acc: None, shutdown: false }
    }

    fn get_event(&self) -> Option<DebouncerEvent> {
        if self.shutdown {
            Some(DebouncerEvent::Shutdown)
        } else if self.acc.is_some() {
            Some(DebouncerEvent::Dirty)
        } else {
            None
        }
    }

    fn should_shutdown(&self) -> bool {
        self.shutdown
    }

    #[inline]
    fn fold<E, F>(&mut self, event_data: E, fold_fn: F)
    where
        F: FnOnce(Option<T>, E) -> T,
    {
        let acc = self.acc.take();
        self.acc = Some(fold_fn(acc, event_data));
    }

    fn swap_acc(&mut self) -> Option<T> {
        self.acc.take()
    }

    fn set_shutdown(&mut self) {
        self.shutdown = true;
    }
}

enum DebouncerEvent {
    /// The debouncer has been told to shut down.
    Shutdown,
    /// The accumulator has been changed.
    Dirty,
}

fn debounce_thread<T>(
    debouncer: Arc<DebouncerController<T>>,
    tx: mpsc::Sender<T>,
    debounce_time: Duration
) {
    let mut guard = debouncer.state.lock().unwrap();

    'debounce: loop {
        let event = 'event: loop {
            // Check if either `acc` or `shutdown` is set.
            if let Some(event) = guard.get_event() {
                break 'event event;
            }

            // If neither `acc` nor `shutdown` have been set, wait for the condvar to be notified
            // then check again. We need to check in a loop because `Condvar` allows spurious
            // wakeups.
            guard = debouncer.main_cvar.wait(guard).unwrap();
        };
    
        match event {
            DebouncerEvent::Shutdown => {
                // Before shutting down, check if there is anything in the accumulator and send it
                // through the channel if it is still open.
                if let Some(acc) = guard.swap_acc() {
                    tx.send(acc).ok();
                }

                break 'debounce;
            },

            DebouncerEvent::Dirty => {
                // Wait for the debounce time, or until the `sleep_cvar` is notified and the
                // `shutdown` boolean is set. Note that checking `shutdown` here is necessary
                // because `Condvar` allows spurious wakeups.
                //
                // By waiting for a condvar, we are temporarily releasing the mutex. This allows
                // the other threads to set `acc` while we are waiting.
                (guard, _) = debouncer.sleep_cvar
                    .wait_timeout_while(guard, debounce_time, |state| {
                        !state.should_shutdown()
                    }).unwrap();

                // Once we have finished waiting, send the contents of the accumulator through the
                // channel.
                if let Some(acc) = guard.swap_acc() {
                    if tx.send(acc).is_err() {
                        // If the other side of the channel has been closed, stop the debouncer.
                        break 'debounce;
                    }
                }

                // Check if the shutdown flag was set while we were waiting, and stop the deboucner
                // if so.
                if guard.should_shutdown() {
                    break 'debounce;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{time::Duration, thread};

    use super::{Debouncer, fold};

    #[test]
    fn test_debounce() {
        let (debouncer, rx) = Debouncer::new(
            Duration::from_millis(50),
            fold::fold_vec_push::<u8>
        ).unwrap();

        for i in 0..3 {
            for j in 0..10 {
                debouncer.debounce(i * 10 + j);
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
        let (debouncer, rx) = Debouncer::new(
            Duration::from_millis(100),
            fold::fold_vec_push::<u8>
        ).unwrap();

        debouncer.debounce(1);
        debouncer.debounce(2);
        debouncer.debounce(3);

        // Drop the debouncer, shutting it down.
        drop(debouncer);

        // Test that the events emitted just before the shutdown are not lost.
        assert_eq!(rx.recv().unwrap(), &[1, 2, 3]);
        assert!(rx.recv().is_err());
    }
}
