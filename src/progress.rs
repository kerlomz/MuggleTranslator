use std::io::{self, Write};
use std::time::Instant;

pub struct ConsoleProgress {
    enabled: bool,
    t0: Instant,
}

impl ConsoleProgress {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            t0: Instant::now(),
        }
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        if !self.enabled {
            return;
        }
        let ts = fmt_elapsed(self.t0.elapsed().as_secs_f64());
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "[{ts}] {}", msg.as_ref());
    }

    pub fn progress(&self, label: &str, current: usize, total: usize) {
        if !self.enabled {
            return;
        }
        let total = total.max(1);
        let current = current.min(total);
        let pct = (current as f64 / total as f64) * 100.0;
        let ts = fmt_elapsed(self.t0.elapsed().as_secs_f64());
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "[{ts}] {label} {current}/{total} ({pct:5.1}%)");
    }
}

fn fmt_elapsed(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
