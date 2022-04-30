# pandet
A lightweight library that helps you detect failure of spawned async tasks without having to `.await` their handles.
Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.

```rust
use pandet::{PanicAlert, OnPanic};

let alert = PanicAlert::new();
let detector = alert.new_detector();

// Whichever async task spawner
task::spawn(
    async move {
        panic!();
    }
    .on_panic(&detector) // 👈
);

assert!(alert.await.is_err());
```

For `!Send` tasks, there is the `UnsendOnPanic` trait:
```rust
use pandet::{PanicAlert, UnsendOnPanic};

let alert = PanicAlert::new();
let detector = alert.new_detector();

task::spawn_local(
    async move {
        panic!();
    }
    .unsend_on_panic(&detector)
);
```

Refined control over how to handle panics can also be implemented with `PanicMonitor`
which works like a stream of alerts. You may also pass some information to the alert/monitor
when a panic occurs:
```rust
use futures::StreamExt;
use pandet::{PanicMonitor, OnPanic};

let mut monitor = PanicMonitor::new();
let detector = monitor.new_detector();

for i in 0..=10 {
    task::spawn(
        async move {
            if i == 10 {
                panic!();
            }
        }
        .on_panic_info(&detector, i)
    );
}

while let Some(res) = monitor.next().await {
    if res.is_err() {
        let id = res.unwrap_err().0;
        assert_eq!(id, 10);
        break;
    }
}
```
