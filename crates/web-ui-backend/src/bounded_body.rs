use anyhow::Result;
use axum::body::Body;
use bytes::Bytes;
use std::any::Any;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

const STREAM_BUFFERED_CHUNKS: usize = 1;
const DEFAULT_MAX_INLINE_STREAM_PRODUCERS: usize = 32;
const DEFAULT_MAX_ATTACHMENT_STREAM_PRODUCERS: usize = 8;
const DEFAULT_STREAM_ADMISSION_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
pub(crate) enum StreamProducerClass {
    Inline,
    Attachment,
}

#[derive(Default)]
struct StreamCancellationState {
    cancelled: AtomicBool,
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
        self.state.cancelled.store(true, Ordering::Relaxed);
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

fn configured_stream_limit(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn stream_producer_slots(class: StreamProducerClass) -> Arc<Semaphore> {
    static INLINE_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static ATTACHMENT_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

    match class {
        StreamProducerClass::Inline => Arc::clone(INLINE_SLOTS.get_or_init(|| {
            Arc::new(Semaphore::new(configured_stream_limit(
                "IRONMESH_WEB_MAX_INLINE_STREAMS",
                DEFAULT_MAX_INLINE_STREAM_PRODUCERS,
            )))
        })),
        StreamProducerClass::Attachment => Arc::clone(ATTACHMENT_SLOTS.get_or_init(|| {
            Arc::new(Semaphore::new(configured_stream_limit(
                "IRONMESH_WEB_MAX_ATTACHMENT_STREAMS",
                DEFAULT_MAX_ATTACHMENT_STREAM_PRODUCERS,
            )))
        })),
    }
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
/// Producers use separate capped pools of dedicated threads so attachment downloads cannot starve
/// inline gallery/video traffic and slow HTTP consumers cannot exhaust Tokio's shared blocking
/// pool. Admission waits only briefly before returning `WouldBlock`, allowing the handler to send
/// a 503 response instead of leaving requests queued indefinitely. Dropping the returned body
/// closes the bounded channel and stops both a blocked writer and cancellation-aware upstream work.
pub(crate) async fn from_blocking_writer<F>(
    expected_length: u64,
    class: StreamProducerClass,
    produce: F,
) -> io::Result<Body>
where
    F: FnOnce(&mut dyn Write, &StreamCancellation) -> Result<()> + Send + 'static,
{
    from_blocking_writer_with_pool(
        expected_length,
        stream_producer_slots(class),
        DEFAULT_STREAM_ADMISSION_TIMEOUT,
        produce,
    )
    .await
}

async fn from_blocking_writer_with_pool<F>(
    expected_length: u64,
    slots: Arc<Semaphore>,
    admission_timeout: Duration,
    produce: F,
) -> io::Result<Body>
where
    F: FnOnce(&mut dyn Write, &StreamCancellation) -> Result<()> + Send + 'static,
{
    let (sender, receiver) = flume::bounded(STREAM_BUFFERED_CHUNKS);
    let cancellation = StreamCancellation::default();
    let producer_cancellation = cancellation.clone();
    let permit = tokio::time::timeout(admission_timeout, slots.acquire_owned())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "binary stream capacity is temporarily exhausted",
            )
        })?
        .map_err(|_| io::Error::other("binary stream capacity pool was closed"))?;

    std::thread::Builder::new()
        .name("ironmesh-web-binary-stream".to_string())
        .spawn(move || {
            let _permit = permit;
            run_producer(sender, producer_cancellation, expected_length, produce);
        })?;

    let cancellation_guard = cancellation.guard();
    Ok(Body::from_stream(async_stream::stream! {
        let _cancellation_guard = cancellation_guard;
        while let Ok(item) = receiver.recv_async().await {
            yield item;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{StreamProducerClass, from_blocking_writer, from_blocking_writer_with_pool};
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
        let mut body = from_blocking_writer(11, StreamProducerClass::Inline, move |writer, _| {
            writer.write_all(b"hello ")?;
            continue_receiver
                .recv()
                .map_err(|error| anyhow!("test gate closed: {error}"))?;
            writer.write_all(b"world")?;
            producer_finished_in_task.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("producer should be admitted");

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
        let mut body = from_blocking_writer(2, StreamProducerClass::Inline, move |writer, _| {
            writer.write_all(b"a")?;
            writer.write_all(b"b")?;
            second_write_finished_in_task.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("producer should be admitted");

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
        let mut body = from_blocking_writer(8, StreamProducerClass::Inline, move |writer, _| {
            writer.write_all(b"partial")?;
            Err(anyhow!("upstream failed"))
        })
        .await
        .expect("producer should be admitted");

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
        let mut body = from_blocking_writer(8, StreamProducerClass::Inline, move |writer, _| {
            writer.write_all(b"partial")?;
            panic!("test producer panic");
        })
        .await
        .expect("producer should be admitted");

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
        let body = from_blocking_writer(
            2,
            StreamProducerClass::Inline,
            move |writer, cancellation| {
                writer.write_all(b"a")?;
                let result = writer.write_all(b"b");
                assert!(result.is_err());
                assert!(cancellation.is_cancelled());
                producer_stopped_in_task.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("producer should be admitted");

        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(body);
        wait_until(|| producer_stopped.load(Ordering::SeqCst)).await;
    }

    #[tokio::test]
    async fn rejects_excess_producers_after_a_bounded_wait() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let first_body = from_blocking_writer_with_pool(
            1,
            Arc::clone(&slots),
            Duration::from_secs(1),
            move |writer, _| {
                release_receiver
                    .recv()
                    .map_err(|error| anyhow!("test gate closed: {error}"))?;
                writer.write_all(b"a")?;
                Ok(())
            },
        )
        .await
        .expect("first producer should be admitted");

        let started = Instant::now();
        let error = from_blocking_writer_with_pool(
            1,
            Arc::clone(&slots),
            Duration::from_millis(25),
            move |writer, _| {
                writer.write_all(b"b")?;
                Ok(())
            },
        )
        .await
        .expect_err("second producer should be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(1));

        release_sender
            .send(())
            .expect("first producer gate should still be open");
        drop(first_body);
    }
}
