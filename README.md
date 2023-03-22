# pandet
[![version](https://img.shields.io/crates/v/pandet)](https://crates.io/crates/pandet)
[![documentation](https://docs.rs/pandet/badge.svg)](https://docs.rs/pandet)  

A lightweight library that helps you detect failure of spawned async tasks without having to `.await` their handles.
Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.

```rust
use pandet::*;

let detector = PanicDetector::new();

// Whichever async task spawner
task::spawn(
    async move {
        panic!();
    }
    .alert(&detector) // 👈 Binds the detector so it is notified of any panic from the future
);

assert!(detector.await.is_some());
```

`!Send` tasks implement the `LocalAlert` trait:
```rust
use pandet::*;

let detector = PanicDetector::new();

task::spawn_local(
    async move {
        // Does some work without panicking...
    }
    .local_alert(&detector)
);

assert!(detector.await.is_none());
```

Refined control over how to handle panics can also be implemented with `PanicMonitor`
which works like a stream of alerts. You may also pass some message to the detector/monitor
when a panic occurs:
```rust
use futures::StreamExt;
use pandet::*;

// Any Unpin + Send + 'static type works
struct FailureMsg {
    task_id: usize,
}

let mut monitor = PanicMonitor::<FailureMsg>::new(); // Or simply PanicMonitor::new()
for task_id in 0..=10 {
    task::spawn(
        async move {
            if task_id % 3 == 0 {
                panic!();
            }
        }
        // Notifies the monitor of the panicked task's ID
        .alert_msg(&monitor, FailureMsg { task_id })
    );
}

let cnt = 0;
while let Some(Panicked(msg)) = monitor.next().await {
    cnt += 1;
    assert_eq!(msg.task_id % 3, 0);
}
assert_eq!(cnt, 4);
```