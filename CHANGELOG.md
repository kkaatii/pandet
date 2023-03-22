# Version 0.4.0

- Rewrite key APIs (again) to improve ergonomics. There is no need to manually drop detectors now.

# Version 0.3.0

- Rewrite key APIs to improve ergonomics.

# Version 0.2.2

- Optimize `Future` and `Stream` implementation of `PanicAlert` and `PanicMonitor`, respectively, so that tasks bounded 
  first will not prevent the panics of tasks bounded later from being detected in a timely manner. 


# Version 0.2.0

- Make `PanicAlert::drop_detector` and `PanicMonitor::drop_detector` return `&mut Self` so they can be directly
  `.await`ed.