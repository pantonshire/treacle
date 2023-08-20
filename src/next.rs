use std::{time::{Instant, Duration}, collections::VecDeque, marker::PhantomData, sync::{Mutex, Condvar, MutexGuard, Arc}};

pub struct DebouncerTx<RawEvent, DebouncedEvent, FoldFn> {
    debouncer: Arc<Debouncer<DebouncedEvent>>,
    fold: FoldFn,
    _raw_event_marker: PhantomData<RawEvent>,
}

impl<RawEvent, DebouncedEvent, FoldFn> DebouncerTx<RawEvent, DebouncedEvent, FoldFn>
where
    FoldFn: Fn(Option<DebouncedEvent>, RawEvent) -> DebouncedEvent,
{
    
}

pub struct DebouncerRx<DebouncedEvent> {
    debouncer: Arc<Debouncer<DebouncedEvent>>
}

impl<DebouncedEvent> DebouncerRx<DebouncedEvent> {
    
}

pub struct DebouncerClosedError;

struct Debouncer<T> {
    state: Mutex<DebouncerState<T>>,
    debounce_time: Duration,
    queue_wait_cvar: Condvar,
    event_ready_wait_cvar: Condvar,
}

impl<T> Debouncer<T> {
    // TODO: error on closed channel?
    fn push<R, F>(&self, raw_event: R, fold: F)
    where
        F: Fn(Option<T>, R) -> T,
    {
        let now = Instant::now();

        let push_outcome = {
            let mut state_guard = self.state.lock().unwrap();
            state_guard.push_latest(raw_event, fold, now, self.debounce_time)
        };
        
        if matches!(push_outcome, PushOutcome::NewAcc) {
            // If we pushed a new event accumulator, wake up one rx thread which was waiting for
            // the queue to be non-empty. Don't wake up all the rx threads because only one of
            // them can consume the acc from the queue; therefore, we should wait until another
            // acc is added to the queue before waking up another thread.
            self.queue_wait_cvar.notify_one();
        }
    }

    fn pop(&self) -> Result<T, DebouncerClosedError> {
        enum PopOutcome<T> {
            Event(T),
            Shutdown,
        }

        let mut state_guard = self.state.lock().unwrap();

        if state_guard.shutdown {
            return Err(DebouncerClosedError);
        }

        let event = loop {
            // If the queue is empty, wait for a tx thread to notify us that a new acc has been
            // added to the queue.
            if state_guard.is_empty() {
                state_guard = self.queue_wait_cvar.wait(state_guard).unwrap();

                // We may have been unparked because someone wants to shut down the debouncer, so
                // check the shutdown flag and return if it is set.
                if state_guard.shutdown {
                    return Err(DebouncerClosedError);
                }
            }

            let event = loop {
                let Some(event) = state_guard.peek_earliest() else {
                    break None;
                };

                let now = Instant::now();
    
                match event.ready_time.checked_duration_since(now) {
                    Some(wait_time) => {
                        // Wait the amount of time between now and the `ready_time` of the event.
                        // This is done using a condvar so the sleep can be interrupted if someone
                        // wants to shut down the debouncer.
                        (state_guard, _) = self.event_ready_wait_cvar
                            .wait_timeout(state_guard, wait_time)
                            .unwrap();

                        // Again, we may have been unparked because someone wants to shut down the
                        // debouncer, so check the shutdown flag and return if it is set.
                        if state_guard.shutdown {
                            return Err(DebouncerClosedError);
                        }
    
                        continue;
                    },
    
                    None => {
                        break state_guard.pop_oldest_acc();
                    },
                }
            };

            let Some(event) = event else {
                continue;
            };
            
            break event;
        };

        // FIXME: replace above with this (set outcome rather than early return)
        let result = 'result: {
            if state_guard.shutdown {
                break 'result PopOutcome::Shutdown;
            }

            // TODO
            todo!()
        };

        match result {
            PopOutcome::Event(event) => Ok(event),
            PopOutcome::Shutdown => {
                state_guard
                    .pop_oldest_acc_discard_none()
                    .ok_or(DebouncerClosedError)
            },
        }
    }

    // TODO: non-blocking `try_pop`?
}

struct DebouncerState<T> {
    event_queue: VecDeque<EventAcc<T>>,
    shutdown: bool,
}

impl<T> DebouncerState<T> {
    fn is_empty(&self) -> bool {
        self.event_queue.is_empty()
    }

    fn peek_oldest(&self) -> Option<&EventAcc<T>> {
        self.event_queue.front()
    }

    fn pop_oldest(&mut self) -> Option<EventAcc<T>> {
        self.event_queue.pop_front()
    }

    fn pop_oldest_acc(&mut self) -> Option<T> {
        self.pop_oldest().and_then(EventAcc::into_acc)
    }

    fn pop_oldest_acc_discard_none(&mut self) -> Option<T> {
        while let Some(event) = self.pop_oldest() {
            if let Some(acc) = event.acc {
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
        match self.event_queue
            .back_mut()
            .and_then(|event| (event.ready_time > now).then_some(event))
        {
            Some(event) => {
                event.fold(raw_event, f);
                PushOutcome::NoNewAcc
            },

            None => {
                let ready_time = now + debounce_time;
                let event = EventAcc::new_from_fold(raw_event, f, ready_time);
                self.event_queue.push_back(event);
                PushOutcome::NewAcc
            },
        }
    }
}

enum PushOutcome {
    NewAcc,
    NoNewAcc,
}

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

