use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Polls `probe` until it yields a value or the timeout expires. Probe errors
/// other than `NotFound` abort the wait.
pub fn poll<T>(
    timeout: Duration,
    interval: Duration,
    what: &str,
    mut probe: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let start = Instant::now();
    loop {
        match probe() {
            Ok(Some(v)) => return Ok(v),
            Ok(None) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        if start.elapsed() >= timeout {
            return Err(Error::Timeout(format!(
                "{what} after {} ms",
                timeout.as_millis()
            )));
        }
        sleep(interval);
    }
}
