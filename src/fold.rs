//! Pre-written fold functions which can be passed to [`Debouncer::new`](crate::Debouncer::new) to
//! specify how raw events should be combined into a single debounced event.

/// A fold function which combines events of type `T` into a `Vec<T>` by pushing them.
/// 
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use treacle::{Debouncer, fold::fold_vec_push};
/// use std::{time::Duration, thread};
/// 
/// let (debouncer, rx) = Debouncer::new(
///     Duration::from_millis(100),
///     fold_vec_push::<u32>
/// )?;
///
/// debouncer.debounce(4);
/// debouncer.debounce(5);
///
/// thread::sleep(Duration::from_millis(150));
/// debouncer.debounce(3);
/// debouncer.debounce(2);
/// debouncer.debounce(1);
///
/// assert_eq!(rx.recv().unwrap(), vec![4, 5]);
/// assert_eq!(rx.recv().unwrap(), vec![3, 2, 1]);
/// # Ok(())
/// # }
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
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use treacle::{Debouncer, fold::fold_unit};
/// use std::{time::Duration, thread};
/// 
/// let (debouncer, rx) = Debouncer::new(
///     Duration::from_millis(100),
///     fold_unit::<u32>
/// )?;
///
/// debouncer.debounce(4);
/// debouncer.debounce(5);
///
/// thread::sleep(Duration::from_millis(150));
/// debouncer.debounce(3);
/// debouncer.debounce(2);
/// debouncer.debounce(1);
///
/// assert_eq!(rx.recv().unwrap(), ());
/// assert_eq!(rx.recv().unwrap(), ());
/// # Ok(())
/// # }
/// ```
pub fn fold_unit<T>(_acc: Option<()>, _e: T) {}
