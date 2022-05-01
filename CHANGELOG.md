# Version 0.2.0

- Make `PanicAlert::drop_detector` and `PanicMonitor::drop_detector` return `&mut Self` so they can be directly
  `.await`ed.