# pandet
[![version](https://img.shields.io/crates/v/pandet)](https://crates.io/crates/pandet)
[![documentation](https://docs.rs/pandet/badge.svg)](https://docs.rs/pandet)  

A lightweight library that helps you detect failure of spawned async tasks without having to `.await` their handles.
Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.

```rust
use pandet::{PanicAlert, OnPanic};

let (alert, detector) = PanicAlert::new();

// Whichever async task spawner
task::spawn(
    async move {
        panic!();
    }
    .on_panic(&detector()) // 👈 Binds the alert's detector
);

drop(detector);
assert!(alert.await.is_some());
```

For `!Send` tasks, there is the `UnsendOnPanic` trait:
```rust
use pandet::{PanicAlert, UnsendOnPanic};

let (alert, detector) = PanicAlert::new();

task::spawn_local(
    async move {
        panic!();
    }
    .unsend_on_panic(&detector)
);

assert!(alert.await.is_some());
```

Refined control over how to handle panics can also be implemented with `PanicMonitor`
which works like a stream of alerts. You may also pass some information to the alert/monitor
when a panic occurs:
```rust
use futures::StreamExt;
use pandet::{PanicMonitor, OnPanic};

// Any Unpin + Send + 'static type works
struct PanicInfo {
    task_id: usize,
}

let (mut monitor, detector) = PanicMonitor::<PanicInfo>::new(); // Or simply PanicMonitor::new()
for task_id in 0..=10 {
    let detector = detector.clone();
    task::spawn(
        async move {
            if task_id % 3 == 0 {
                panic!();
            }
        }
        // Informs the monitor of which task panicked
        .on_panic_info(&detector, PanicInfo { task_id })
    );
}

drop(detector);
let cnt = 0;
while let Some(res) = monitor.next().await {
    cnt += 1;
    let info = res.0;
    assert_eq!(info.task_id % 3, 0);
}
assert_eq!(cnt, 4);
```