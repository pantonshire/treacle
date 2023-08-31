use std::{panic, thread, time::Duration};

use treacle::{fold, debouncer};

fn main() {
    // Create a debouncer which combines `u32` events which occur within the same 500ms window by
    // pushing them to a `Vec`.
    let (tx, rx) = debouncer(Duration::from_millis(500), fold::fold_vec_push::<u32>);

    // Spawn a thread which receives the debounced events and prints them.
    let t = thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            println!("{:?}", event);
        }
    });

    // Create 100 raw events for the debouncer to debounce.
    for i in 0..100 {
        tx.send(i).unwrap();
        thread::sleep(Duration::from_millis(50));
    }

    // Drop the tx, so there are no more `DebouncerTx`s left. This will gracefully shut down the
    // debouncer, stopping the loop in the other thread once it has received all the events.
    drop(tx);

    if let Err(err) = t.join() {
        panic::resume_unwind(err);
    }
}
