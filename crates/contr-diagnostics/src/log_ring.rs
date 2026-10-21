use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// The most recent output of a `tracing` formatter, at most a fixed number of bytes; the oldest writes go first.
#[derive(Clone)]
pub struct LogRing(Arc<Mutex<Ring>>);

struct Ring {
    writes: VecDeque<String>,
    len: usize,
    capacity: usize,
}

impl LogRing {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(Ring {
            writes: VecDeque::new(),
            len: 0,
            capacity,
        })))
    }

    pub fn contents(&self) -> String {
        self.0
            .lock()
            .map(|ring| ring.writes.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

impl fmt::Debug for LogRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogRing").finish_non_exhaustive()
    }
}

impl io::Write for LogRing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut ring) = self.0.lock() {
            let write = String::from_utf8_lossy(buf).into_owned();
            ring.len += write.len();
            ring.writes.push_back(write);
            while ring.len > ring.capacity {
                let Some(oldest) = ring.writes.pop_front() else {
                    break;
                };
                ring.len -= oldest.len();
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogRing {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn keeps_the_newest_writes_within_capacity() {
        let mut ring = LogRing::new(12);

        for line in ["first\n", "second\n", "third\n"] {
            ring.write_all(line.as_bytes()).unwrap();
        }

        assert_eq!(ring.contents(), "third\n");
        ring.write_all(b"4\n").unwrap();
        assert_eq!(ring.contents(), "third\n4\n");
    }

    #[test]
    fn records_what_a_subscriber_formats() {
        let ring = LogRing::new(4096);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(ring.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || tracing::info!(state = "Running", "engine state changed"));

        assert!(ring.contents().contains("INFO"));
        assert!(ring.contents().contains("engine state changed state=\"Running\""));
    }
}
