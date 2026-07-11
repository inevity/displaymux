use serde::Serialize;
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
};
use tracing_subscriber::fmt::MakeWriter;

// Bounds queued log payloads to 16 MiB while retaining a large transition burst.
const LOG_QUEUE_CAPACITY: usize = 1_024;
const LOG_RECORD_MAX_BYTES: usize = 16 * 1_024;

#[derive(Clone)]
pub struct RuntimeMetrics {
    inner: Arc<RuntimeMetricsInner>,
}

struct RuntimeMetricsInner {
    log_queue_depth: AtomicUsize,
    dropped_logs: AtomicU64,
    reconnect_consecutive: AtomicU64,
    reconnect_backoff_ms: AtomicU64,
    retry_alert: AtomicBool,
}

impl RuntimeMetrics {
    fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeMetricsInner {
                log_queue_depth: AtomicUsize::new(0),
                dropped_logs: AtomicU64::new(0),
                reconnect_consecutive: AtomicU64::new(0),
                reconnect_backoff_ms: AtomicU64::new(0),
                retry_alert: AtomicBool::new(false),
            }),
        }
    }

    pub fn record_reconnect_failure(
        &self,
        consecutive_failures: u64,
        backoff_ms: u64,
        alert_after: u64,
    ) {
        self.inner
            .reconnect_consecutive
            .store(consecutive_failures, Ordering::Relaxed);
        self.inner
            .reconnect_backoff_ms
            .store(backoff_ms, Ordering::Relaxed);
        self.inner
            .retry_alert
            .store(consecutive_failures >= alert_after, Ordering::Relaxed);
    }

    pub fn record_synchronized(&self) {
        self.inner.reconnect_consecutive.store(0, Ordering::Relaxed);
        self.inner.reconnect_backoff_ms.store(0, Ordering::Relaxed);
        self.inner.retry_alert.store(false, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            log_queue_depth: self.inner.log_queue_depth.load(Ordering::Relaxed),
            log_queue_capacity: LOG_QUEUE_CAPACITY,
            log_record_max_bytes: LOG_RECORD_MAX_BYTES,
            dropped_logs: self.inner.dropped_logs.load(Ordering::Relaxed),
            reconnect_consecutive: self.inner.reconnect_consecutive.load(Ordering::Relaxed),
            reconnect_backoff_ms: self.inner.reconnect_backoff_ms.load(Ordering::Relaxed),
            retry_alert: self.inner.retry_alert.load(Ordering::Relaxed),
        }
    }
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RuntimeSnapshot {
    pub log_queue_depth: usize,
    pub log_queue_capacity: usize,
    pub log_record_max_bytes: usize,
    pub dropped_logs: u64,
    pub reconnect_consecutive: u64,
    pub reconnect_backoff_ms: u64,
    pub retry_alert: bool,
}

#[derive(Clone)]
struct BoundedLogWriter {
    sender: SyncSender<Vec<u8>>,
    metrics: RuntimeMetrics,
    record_max_bytes: usize,
}

struct LogRecord {
    sender: SyncSender<Vec<u8>>,
    metrics: RuntimeMetrics,
    bytes: Vec<u8>,
    record_max_bytes: usize,
    oversized: bool,
}

impl<'a> MakeWriter<'a> for BoundedLogWriter {
    type Writer = LogRecord;

    fn make_writer(&'a self) -> Self::Writer {
        LogRecord {
            sender: self.sender.clone(),
            metrics: self.metrics.clone(),
            bytes: Vec::new(),
            record_max_bytes: self.record_max_bytes,
            oversized: false,
        }
    }
}

impl Write for LogRecord {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.oversized {
            if self.bytes.len().saturating_add(bytes.len()) <= self.record_max_bytes {
                self.bytes.extend_from_slice(bytes);
            } else {
                self.bytes.clear();
                self.oversized = true;
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogRecord {
    fn drop(&mut self) {
        if self.oversized {
            self.metrics
                .inner
                .dropped_logs
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.bytes.is_empty() {
            return;
        }

        self.metrics
            .inner
            .log_queue_depth
            .fetch_add(1, Ordering::Relaxed);
        if self
            .sender
            .try_send(std::mem::take(&mut self.bytes))
            .is_err()
        {
            self.metrics
                .inner
                .log_queue_depth
                .fetch_sub(1, Ordering::Relaxed);
            self.metrics
                .inner
                .dropped_logs
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn init_logging() -> RuntimeMetrics {
    let metrics = RuntimeMetrics::new();
    let writer = spawn_writer(
        LOG_QUEUE_CAPACITY,
        LOG_RECORD_MAX_BYTES,
        io::stdout(),
        metrics.clone(),
    );
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_current_span(false)
        .with_writer(writer)
        .init();
    metrics
}

fn spawn_writer<W>(
    queue_capacity: usize,
    record_max_bytes: usize,
    mut sink: W,
    metrics: RuntimeMetrics,
) -> BoundedLogWriter
where
    W: Write + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(queue_capacity);
    let worker_metrics = metrics.clone();
    std::thread::Builder::new()
        .name("tv-multiview-log".to_string())
        .spawn(move || {
            while let Ok(record) = receiver.recv() {
                worker_metrics
                    .inner
                    .log_queue_depth
                    .fetch_sub(1, Ordering::Relaxed);
                if sink.write_all(&record).is_err() {
                    worker_metrics
                        .inner
                        .dropped_logs
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .expect("log worker thread must start");
    BoundedLogWriter {
        sender,
        metrics,
        record_max_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc::{channel, Receiver, Sender},
        time::{Duration, Instant},
    };

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
    fn stalled_sink_drops_records_without_blocking_producer() {
        let metrics = RuntimeMetrics::new();
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let writer = spawn_writer(
            1,
            64,
            BlockingSink {
                entered: entered_tx,
                release: release_rx,
            },
            metrics.clone(),
        );

        drop(write_record(&writer, b"first\n"));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        for _ in 0..100 {
            drop(write_record(&writer, b"queued\n"));
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(metrics.snapshot().dropped_logs > 0);
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn oversized_record_is_dropped_before_allocation_grows_past_bound() {
        let metrics = RuntimeMetrics::new();
        let writer = spawn_writer(1, 4, io::sink(), metrics.clone());
        drop(write_record(&writer, b"12345"));
        assert_eq!(metrics.snapshot().dropped_logs, 1);
    }

    fn write_record(writer: &BoundedLogWriter, bytes: &[u8]) -> LogRecord {
        let mut record = writer.make_writer();
        record.write_all(bytes).unwrap();
        record
    }
}
