# `treacle`
A generic event debouncer for Rust, for grouping events which occur close together in time into a
single event.

For example, you may have some
[templates](https://www.arewewebyet.org/topics/templating/)
which you want to reload whenever a template file changes. However, if many small changes are made
to the template files in quick succession, it would be wasteful to reload the templates for every
change; instead, a debouncer could be used to group the changes that occur at a similar time into
a single change, so the templates are only reloaded once.

A new debouncer can be created with the
`debouncer`
function, which returns the debouncer in two halves: a tx (send) half and rx (receive) half. The
tx sends raw un-debounced events to the debouncer, and the rx receives the debounced events from
the debouncer. Both halves can be cloned to allow for multiple senders and receivers.

```rust
use std::{thread, time::Duration};

// Create a new debouncer which takes raw events of type `u32` and outputs
// debounced events of type `Vec<u32>`.
let (tx, rx) = treacle::debouncer::<u32, Vec<u32>, _>(
    // Group events which occur in the same 500ms window.
    Duration::from_millis(500),
    // Combine raw events by pushing them to a vector.
    |acc, raw_event| {
        let mut events_vector = acc.unwrap_or_default();
        events_vector.push(raw_event);
        events_vector
    });

thread::spawn(move || {
    // Send two raw events in quick succession.
    tx.send(10).unwrap();
    tx.send(20).unwrap();

    // Wait, then send more raw events.
    thread::sleep(Duration::from_millis(500));
    tx.send(30).unwrap();
    tx.send(40).unwrap();
    tx.send(50).unwrap();
});

assert_eq!(rx.recv().unwrap(), &[10, 20]);
assert_eq!(rx.recv().unwrap(), &[30, 40, 50]);
```
