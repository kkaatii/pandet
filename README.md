# pandet
[![version](https://img.shields.io/crates/v/pandet)](https://crates.io/crates/pandet)
[![documentation](https://docs.rs/pandet/badge.svg)](https://docs.rs/pandet)  

A lightweight library that helps you detect failure of spawned async tasks without having to `.await` their handles.
Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.

```rust
use pandet::{PanicAlert, OnPanic};

let mut alert = PanicAlert::new();

// Whichever async task spawner
task::spawn(
    async move {
        panic!();
    }
    .on_panic(&alert.new_detector()) // 👈 Binds the alert's detector
);

assert!(alert.drop_detector().await.is_err());  // See notes below
```
IMPORTANT NOTE: Directly `.await`ing an alert is possible, but in this case the alert as a
future will only finish when a task panics. Calling `drop_detector()` allows it to finish with
a `Ok(())` if no task panics as long as all the other `PanicDetector`s paired with the alert
has gone out of scope. See [`PanicAlert::drop_detector`] and [`PanicMonitor::drop_detector`]
for more details.

For `!Send` tasks, there is the `UnsendOnPanic` trait:
```rust
use pandet::{PanicAlert, UnsendOnPanic};

let mut alert = PanicAlert::new();

task::spawn_local(
    async move {
        panic!();
    }
    .unsend_on_panic(&alert.new_detector())
);

assert!(alert.drop_detector().await.is_err());
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

let mut monitor = PanicMonitor::<PanicInfo>::new(); // Or simply PanicMonitor::new()
{
    let detector = monitor.new_detector();
    for task_id in 0..=10 {
        task::spawn(
            async move {
                if task_id == 10 {
                    panic!();
                }
            }
            // Informs the monitor of which task panicked
            .on_panic_info(&detector, PanicInfo { task_id })
        );
    }
} // detector goes out of scope, allowing the monitor to finish after calling drop_detector()

while let Some(res) = monitor.drop_detector().next().await {
    if let Err(e) = res {
        let info = e.0;
        assert_eq!(info.task_id, 10);
        break;
    }
}
```