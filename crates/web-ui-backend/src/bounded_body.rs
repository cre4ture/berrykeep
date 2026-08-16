use anyhow::Result;
use axum::body::Body;
use bytes::Bytes;
use std::any::Any;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, Semaphore};

const STREAM_BUFFERED_CHUNKS: usize = 1;
const MAX_ACTIVE_STREAM_PRODUCERS: usize = 16;

#[derive(Default)]
struct StreamCancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Clone, Default)]
pub(crate) struct StreamCancellation {
    state: Arc<StreamCancellationState>,
}

impl StreamCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Relaxed)
    }

    fn guard(&self) -> StreamCancellationGuard {
        StreamCancellationGuard {
            cancellation: self.clone(),
        }
    }

    fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::Relaxed) {
            self.state.notify.notify_waiters();
        }
    }

    async fn cancelled(&self) {
        let notified = self.state.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

struct StreamCancellationGuard {
    cancellation: StreamCancellation,
}

impl Drop for StreamCancellationGuard {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

struct BoundedBodyWriter {
    sender: flume::Sender<io::Result<Bytes>>,
    cancellation: StreamCancellation,
    bytes_written: u64,
}

impl BoundedBodyWriter {
    fn send_error(&self, error: io::Error) {
        let _ = self.sender.send(Err(error));
    }
}

impl Write for BoundedBodyWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "response body was cancelled",
            ));
        }

        self.sender
            .send(Ok(Bytes::copy_from_slice(buffer)))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "response body receiver was dropped",
                )
            })?;
        self.bytes_written = self.bytes_written.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "response body was cancelled",
            ));
        }
        Ok(())
    }
}

fn stream_producer_slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_ACTIVE_STREAM_PRODUCERS))))
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}

fn run_producer<F>(
    sender: flume::Sender<io::Result<Bytes>>,
    cancellation: StreamCancellation,
    expected_length: u64,
    produce: F,
) where
    F: FnOnce(&mut dyn Write, &StreamCancellation) -> Result<()>,
{
    let mut writer = BoundedBodyWriter {
        sender,
        cancellation: cancellation.clone(),
        bytes_written: 0,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        produce(&mut writer, &cancellation)
    }));
    if cancellation.is_cancelled() {
        return;
    }

    match result {
        Ok(Err(error)) => writer.send_error(io::Error::other(format!("{error:#}"))),
        Ok(Ok(())) if writer.bytes_written != expected_length => {
            writer.send_error(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "stream producer wrote {} bytes, expected {expected_length}",
                    writer.bytes_written
                ),
            ));
        }
        Ok(Ok(())) => {}
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(panic = message, "binary stream producer panicked");
            writer.send_error(io::Error::other(format!(
                "binary stream producer panicked: {message}"
            )));
        }
    }
}

/// Adapts a blocking writer producer to an HTTP body with one-chunk backpressure.
///
/// Producers use capped, dedicated threads so slow HTTP consumers cannot exhaust Tokio's shared
/// blocking pool. Dropping the returned body cancels queued work, closes the bounded channel, and
/// stops both a blocked writer and cancellation-aware upstream work promptly.
pub(crate) fn from_blocking_writer<F>(expected_length: u64, produce: F) -> Body
where
    F: FnOnce(&mut dyn Write, &StreamCancellation) -> Result<()> + Send + 'static,
{
    let (sender, receiver) = flume::bounded(STREAM_BUFFERED_CHUNKS);
    let cancellation = StreamCancellation::default();
    let producer_cancellation = cancellation.clone();

    tokio::spawn(async move {
        let cancellation_waiter = producer_cancellation.clone();
        let permit = tokio::select! {
            _ = cancellation_waiter.cancelled() => return,
            permit = stream_producer_slots().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
        };
        if producer_cancellation.is_cancelled() {
            return;
        }

        let thread_cancellation = producer_cancellation.clone();
        let thread_sender = sender.clone();
        let spawn_result = std::thread::Builder::new()
            .name("ironmesh-web-binary-stream".to_string())
            .spawn(move || {
                let _permit = permit;
                run_producer(thread_sender, thread_cancellation, expected_length, produce);
            });
        if let Err(error) = spawn_result {
            tracing::error!(error = %error, "failed to start binary stream producer thread");
            let _ = sender
                .send_async(Err(io::Error::other(format!(
                    "failed to start binary stream producer: {error}"
                ))))
                .await;
        }
    });

    let cancellation_guard = cancellation.guard();
    Body::from_stream(async_stream::stream! {
        let _cancellation_guard = cancellation_guard;
        while let Ok(item) = receiver.recv_async().await {
            yield item;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::from_blocking_writer;
    use anyhow::anyhow;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    async fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not become true");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn yields_bytes_before_the_producer_finishes() {
        let (continue_sender, continue_receiver) = std::sync::mpsc::sync_channel(0);
        let producer_finished = Arc::new(AtomicBool::new(false));
        let producer_finished_in_task = Arc::clone(&producer_finished);
        let mut body = from_blocking_writer(11, move |writer, _| {
            writer.write_all(b"hello ")?;
            continue_receiver
                .recv()
                .map_err(|error| anyhow!("test gate closed: {error}"))?;
            writer.write_all(b"world")?;
            producer_finished_in_task.store(true, Ordering::SeqCst);
            Ok(())
        });

        let first = body
            .frame()
            .await
            .expect("body should contain a frame")
            .expect("first frame should succeed");
        assert_eq!(
            first
                .data_ref()
                .expect("frame should contain data")
                .as_ref(),
            b"hello "
        );
        assert!(!producer_finished.load(Ordering::SeqCst));

        continue_sender
            .send(())
            .expect("producer gate should still be open");
        let second = body
            .frame()
            .await
            .expect("body should contain a second frame")
            .expect("second frame should succeed");
        assert_eq!(
            second
                .data_ref()
                .expect("frame should contain data")
                .as_ref(),
            b"world"
        );
    }

    #[tokio::test]
    async fn applies_one_chunk_of_backpressure() {
        let second_write_finished = Arc::new(AtomicBool::new(false));
        let second_write_finished_in_task = Arc::clone(&second_write_finished);
        let mut body = from_blocking_writer(2, move |writer, _| {
            writer.write_all(b"a")?;
            writer.write_all(b"b")?;
            second_write_finished_in_task.store(true, Ordering::SeqCst);
            Ok(())
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!second_write_finished.load(Ordering::SeqCst));

        let first = body
            .frame()
            .await
            .expect("body should contain a frame")
            .expect("first frame should succeed");
        assert_eq!(
            first
                .data_ref()
                .expect("frame should contain data")
                .as_ref(),
            b"a"
        );
        wait_until(|| second_write_finished.load(Ordering::SeqCst)).await;
    }

    #[tokio::test]
    async fn reports_producer_failures_after_delivered_bytes() {
        let mut body = from_blocking_writer(8, move |writer, _| {
            writer.write_all(b"partial")?;
            Err(anyhow!("upstream failed"))
        });

        let first = body
            .frame()
            .await
            .expect("body should contain a frame")
            .expect("first frame should succeed");
        assert_eq!(
            first
                .data_ref()
                .expect("frame should contain data")
                .as_ref(),
            b"partial"
        );
        let error = body
            .frame()
            .await
            .expect("body should contain an error frame")
            .expect_err("producer failure should reach the body");
        assert!(error.to_string().contains("upstream failed"));
    }

    #[tokio::test]
    async fn reports_producer_panics_after_delivered_bytes() {
        let mut body = from_blocking_writer(8, move |writer, _| {
            writer.write_all(b"partial")?;
            panic!("test producer panic");
        });

        let first = body
            .frame()
            .await
            .expect("body should contain a frame")
            .expect("first frame should succeed");
        assert_eq!(
            first
                .data_ref()
                .expect("frame should contain data")
                .as_ref(),
            b"partial"
        );
        let error = body
            .frame()
            .await
            .expect("body should contain an error frame")
            .expect_err("producer panic should reach the body");
        assert!(error.to_string().contains("test producer panic"));
    }

    #[tokio::test]
    async fn dropping_the_body_cancels_a_blocked_producer() {
        let producer_stopped = Arc::new(AtomicBool::new(false));
        let producer_stopped_in_task = Arc::clone(&producer_stopped);
        let body = from_blocking_writer(2, move |writer, cancellation| {
            writer.write_all(b"a")?;
            let result = writer.write_all(b"b");
            assert!(result.is_err());
            assert!(cancellation.is_cancelled());
            producer_stopped_in_task.store(true, Ordering::SeqCst);
            Ok(())
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(body);
        wait_until(|| producer_stopped.load(Ordering::SeqCst)).await;
    }
}
