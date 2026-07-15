use env_logger::{Env, Target};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
};

// Bounds queued payloads to 16 MiB while retaining a large diagnostic burst.
const LOG_QUEUE_CAPACITY: usize = 1_024;
const LOG_RECORD_MAX_BYTES: usize = 16 * 1_024;

struct BoundedLogWriter {
    sender: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    record_max_bytes: usize,
}

impl Write for BoundedLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.record_max_bytes {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes.len());
        }
        if self.sender.try_send(bytes.to_vec()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn init(env: Env<'_>) {
    let writer = spawn_writer(LOG_QUEUE_CAPACITY, LOG_RECORD_MAX_BYTES, io::stderr());
    env_logger::Builder::from_env(env)
        .target(Target::Pipe(Box::new(writer)))
        .init();
}

fn spawn_writer<W>(queue_capacity: usize, record_max_bytes: usize, mut sink: W) -> BoundedLogWriter
where
    W: Write + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(queue_capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    let worker_dropped = dropped.clone();
    std::thread::Builder::new()
        .name("lan-mouse-log".to_string())
        .spawn(move || {
            let mut reported_dropped = 0;
            while let Ok(record) = receiver.recv() {
                let dropped_total = worker_dropped.load(Ordering::Relaxed);
                if dropped_total > reported_dropped {
                    let summary = format!(
                        "lan-mouse log sink dropped {dropped_total} records before recovery\n"
                    );
                    if sink.write_all(summary.as_bytes()).is_ok() {
                        reported_dropped = dropped_total;
                    }
                }
                if sink.write_all(&record).is_err() {
                    worker_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .expect("log worker thread must start");
    BoundedLogWriter {
        sender,
        dropped,
        record_max_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Mutex, Once,
            mpsc::{Receiver, Sender, channel},
        },
        time::{Duration, Instant},
    };

    struct CaptureLogger {
        records: Mutex<Vec<String>>,
    }

    impl log::Log for CaptureLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                self.records.lock().unwrap().push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    static CAPTURE_LOGGER: CaptureLogger = CaptureLogger {
        records: Mutex::new(Vec::new()),
    };
    static CAPTURE_LOGGER_INIT: Once = Once::new();

    fn reset_capture_logger() {
        CAPTURE_LOGGER_INIT.call_once(|| {
            log::set_logger(&CAPTURE_LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Info);
        });
        CAPTURE_LOGGER.records.lock().unwrap().clear();
    }

    struct BlockingSink {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl Write for BlockingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_sink_drops_records_without_blocking_service_thread() {
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let mut writer = spawn_writer(
            1,
            64,
            BlockingSink {
                entered: entered_tx,
                release: release_rx,
            },
        );

        writer.write_all(b"first\n").unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        for _ in 0..100 {
            writer.write_all(b"queued\n").unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(writer.dropped.load(Ordering::Relaxed) > 0);
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn oversized_record_is_counted_and_not_queued() {
        let mut writer = spawn_writer(1, 4, io::sink());
        writer.write_all(b"12345").unwrap();
        assert_eq!(writer.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn tracing_clipboard_events_reach_log_facade() {
        reset_capture_logger();

        tracing::info!(
            event = "clipboard_backend_ready",
            reason = "ready",
            "native clipboard actor ready"
        );

        let records = CAPTURE_LOGGER.records.lock().unwrap();
        assert!(records.iter().any(|record| {
            record.contains("clipboard_backend_ready")
                && record.contains("native clipboard actor ready")
        }));
    }
}
