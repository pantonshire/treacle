use std::{
    sync::{Arc, Mutex, Condvar, mpsc},
    time::Duration,
    thread::{self, JoinHandle},
    io,
    panic,
};

/// A debouncer for deduplicating groups of events which occur at a similar time. Upon receiving an
/// event (via `DebouncerController::notify_dirty`), the debouncer waits for a specified duration,
/// during which time any additional events will be considered part of the same group. Once it has
/// finished waiting, it will emit a single event via a `mpsc` channel.
pub struct Debouncer<T> {
    thread: Option<JoinHandle<()>>,
    controller: Arc<DebouncerController<T>>,
}

impl<T> Debouncer<T>
where
    T: Send + 'static,
{
    pub fn start_new(debounce_time: Duration) -> Result<(Self, mpsc::Receiver<T>), io::Error> {
        let controller = Arc::new(DebouncerController {
            state: Mutex::new(DebouncerState::new()),
            main_cvar: Condvar::new(),
            sleep_cvar: Condvar::new(),
        });
        
        let (tx, rx) = mpsc::channel::<T>();

        let thread = thread::Builder::new().spawn({
            let debounce_state = controller.clone();
            move || debounce_thread(debounce_state, tx, debounce_time)
        })?;
        
        Ok((Self { thread: Some(thread), controller }, rx))
    }
}

impl<T> Debouncer<T> {
    pub fn controller(&self) -> &Arc<DebouncerController<T>> {
        &self.controller
    }
}

impl<T> Drop for Debouncer<T> {
    fn drop(&mut self) {
        self.controller().notify_shutdown();

        let thread = self.thread
            .take()
            .expect("debouncer thread has already been shut down");
        
        match thread.join() {
            Ok(()) => (),
            Err(err) => panic::resume_unwind(err),
        }
    }
}

pub struct DebouncerController<T> {
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
    /// ```no_run
    /// |acc, e| {
    ///     let mut acc = acc.unwrap_or_default();
    ///     acc.push(e);
    ///     acc
    /// }
    /// ```
    pub fn notify_event<E, F>(&self, event_data: E, f: F)
    where
        F: FnOnce(Option<T>, E) -> T,
    {
        let mut guard = self.state.lock().unwrap();
        guard.fold(event_data, f);
        drop(guard);
        self.main_cvar.notify_one();
    }

    fn notify_shutdown(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.set_shutdown();
        drop(guard);
        self.main_cvar.notify_one();
        self.sleep_cvar.notify_one();
    }
}

impl<T> DebouncerController<Vec<T>> {
    pub fn notify_event_push(&self, event_data: T) {
        self.notify_event(event_data, |acc, event_data| {
            let mut acc = acc.unwrap_or_default();
            acc.push(event_data);
            acc
        });
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

    fn fold<E, F>(&mut self, event_data: E, f: F)
    where
        F: FnOnce(Option<T>, E) -> T,
    {
        let acc = self.acc.take();
        self.acc = Some(f(acc, event_data));
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
            DebouncerEvent::Shutdown => break 'debounce,

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

                if guard.should_shutdown() {
                    break 'debounce;
                }

                if let Some(acc) = guard.swap_acc() {
                    if tx.send(acc).is_err() {
                        break 'debounce
                    }
                }
            },
        }
    }
}
