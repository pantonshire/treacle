//! Pre-written fold functions which can be passed to [`debouncer`](crate::debouncer) to
//! specify how raw events should be combined into a single debounced event.

/// A fold function which combines events of type `T` into a `Vec<T>` by pushing them.
/// 
/// ```
/// # use treacle::{debouncer, fold::fold_vec_push};
/// use std::{time::Duration, thread};
/// 
/// let (tx, rx) = debouncer(Duration::from_millis(100), fold_vec_push::<u32>);
///
/// tx.send(4).unwrap();
/// tx.send(5).unwrap();
///
/// thread::sleep(Duration::from_millis(150));
/// tx.send(3).unwrap();
/// tx.send(2).unwrap();
/// tx.send(1).unwrap();
///
/// assert_eq!(rx.recv().unwrap(), vec![4, 5]);
/// assert_eq!(rx.recv().unwrap(), vec![3, 2, 1]);
/// ```
pub fn fold_vec_push<T>(acc: Option<Vec<T>>, e: T) -> Vec<T> {
    let mut acc = acc.unwrap_or_default();
    acc.push(e);
    acc
}

/// A fold function which discards all raw event data. Useful when you are just interested in the
/// existence of events and not any data associated with them.
/// 
/// ```
/// # use treacle::{debouncer, fold::fold_unit};
/// use std::{time::Duration, thread};
/// 
/// let (tx, rx) = debouncer(Duration::from_millis(100), fold_unit::<u32>);
///
/// tx.send(4).unwrap();
/// tx.send(5).unwrap();
///
/// thread::sleep(Duration::from_millis(150));
/// tx.send(3).unwrap();
/// tx.send(2).unwrap();
/// tx.send(1).unwrap();
///
/// assert_eq!(rx.recv().unwrap(), ());
/// assert_eq!(rx.recv().unwrap(), ());
/// ```
pub fn fold_unit<T>(_acc: Option<()>, _e: T) {}
