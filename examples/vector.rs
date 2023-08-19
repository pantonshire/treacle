use std::{error::Error, panic, thread, time::Duration};

use treacle::{fold, Debouncer};

fn main() -> Result<(), Box<dyn Error>> {
    // Create a debouncer which combines `u32` events which occur within the same 500ms window by
    // pushing them to a `Vec`.
    let (debouncer, rx) =
        Debouncer::new(Duration::from_millis(500), fold::fold_vec_push::<u32>)?;

    // Spawn a thread which receives the debounced events and prints them.
    let t = thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            println!("{:?}", event);
        }
    });

    // Create 100 raw events for the debouncer to debounce.
    for i in 0..100 {
        debouncer.debounce(i);
        thread::sleep(Duration::from_millis(50));
    }

    // Dropping the debouncer will gracefully close the mpsc channel, stopping the loop in the
    // other thread.
    drop(debouncer);

    if let Err(err) = t.join() {
        panic::resume_unwind(err);
    }

    Ok(())
}
