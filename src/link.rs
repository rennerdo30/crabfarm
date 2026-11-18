//! A framed connection split into a reader task and a writer task, so the
//! owner can `select!` on incoming messages without dropping a half-read frame
//! (`FramedConn::recv` is not cancel-safe).
//!
//! Dropping a `Link` drops its sender, so the writer drains its queue and
//! closes the write half (the peer sees EOF), and it aborts the reader task:
//! a reader blocked on a peer that never speaks would otherwise keep the
//! connection's read half, and the socket, open for good. `close` does the
//! same but waits for the queue to be flushed.

use crate::proto::Message;
use crate::transport::FramedConn;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Frames queued per direction before the sender waits (backpressure).
const QUEUE: usize = 64;

/// Last frame seen in either direction, and the idle limit (0 = none).
struct Activity {
    base: Instant,
    last_ms: AtomicU64,
    limit_ms: AtomicU64,
}

impl Activity {
    fn touch(&self) {
        self.last_ms
            .store(self.base.elapsed().as_millis() as u64, Ordering::Release);
    }

    fn idle_for(&self) -> Duration {
        let last = self.last_ms.load(Ordering::Acquire);
        Duration::from_millis((self.base.elapsed().as_millis() as u64).saturating_sub(last))
    }
}

pub struct Link {
    pub tx: mpsc::Sender<Message>,
    pub rx: mpsc::Receiver<Message>,
    writer: Option<JoinHandle<()>>,
    reader: JoinHandle<()>,
    activity: Arc<Activity>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // The writer is left to drain its queue (its sender is dropped with
        // `self`); only the reader can block for ever.
        self.reader.abort();
    }
}

impl Link {
    pub fn new<S>(conn: FramedConn<S>) -> Link
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Link::new_with_idle(conn, None)
    }

    /// Like [`Link::new`], with a watchdog that ends the connection (the
    /// owner then sees `recv() == None`) when no frame crossed it in either
    /// direction for `idle`. [`Link::set_idle`] changes or lifts the limit.
    pub fn new_with_idle<S>(conn: FramedConn<S>, idle: Option<Duration>) -> Link
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let activity = Arc::new(Activity {
            base: Instant::now(),
            last_ms: AtomicU64::new(0),
            limit_ms: AtomicU64::new(idle.map_or(0, |d| d.as_millis().max(1) as u64)),
        });
        let (rd, wr) = tokio::io::split(conn.stream);
        let mut rd = FramedConn::new(rd);
        let mut wr = FramedConn::new(wr);
        let (tx, mut out_rx) = mpsc::channel::<Message>(QUEUE);
        let (in_tx, rx) = mpsc::channel::<Message>(QUEUE);
        let act_w = activity.clone();
        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                act_w.touch();
                if let Err(e) = wr.send(&m).await {
                    tracing::debug!("link write failed: {e:#}");
                    break;
                }
            }
            // Closing the write half lets the peer see a clean EOF.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr.stream).await;
        });
        let act_r = activity.clone();
        let reader = tokio::spawn(async move {
            loop {
                match rd.recv().await {
                    Ok(Some(m)) => {
                        act_r.touch();
                        if in_tx.send(m).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("link read failed: {e:#}");
                        break;
                    }
                }
            }
        });
        if idle.is_some() {
            // The reader cannot time out itself (`recv` is not cancel-safe), so
            // a watchdog aborts it; the owner then sees the connection end.
            let act = activity.clone();
            let reader_handle = reader.abort_handle();
            tokio::spawn(async move {
                loop {
                    if reader_handle.is_finished() {
                        return;
                    }
                    let limit = act.limit_ms.load(Ordering::Acquire);
                    let pause = if limit == 0 {
                        Duration::from_secs(5)
                    } else {
                        let limit = Duration::from_millis(limit);
                        let idle = act.idle_for();
                        if idle >= limit {
                            tracing::debug!("connection idle for {} s, closing", idle.as_secs());
                            reader_handle.abort();
                            return;
                        }
                        (limit - idle).min(Duration::from_secs(5))
                    };
                    tokio::time::sleep(pause).await;
                }
            });
        }
        Link {
            tx,
            rx,
            writer: Some(writer),
            reader,
            activity,
        }
    }

    /// Change or lift (`None`) the idle limit of a link made with
    /// [`Link::new_with_idle`]; the idle clock restarts.
    pub fn set_idle(&self, idle: Option<Duration>) {
        self.activity.touch();
        self.activity.limit_ms.store(
            idle.map_or(0, |d| d.as_millis().max(1) as u64),
            Ordering::Release,
        );
    }

    /// Queue a frame. Fails once the writer task has stopped (connection gone).
    pub async fn send(&self, m: Message) -> anyhow::Result<()> {
        self.tx
            .send(m)
            .await
            .map_err(|_| anyhow::anyhow!("connection closed"))
    }

    /// Next frame; `None` when the connection ended.
    pub async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await
    }

    /// Flush everything queued, then close.
    pub async fn close(mut self) {
        // Replacing the channel ends drops the real ones: the writer then
        // drains its queue and shuts the write half down.
        drop(std::mem::replace(&mut self.tx, mpsc::channel(1).0));
        drop(std::mem::replace(&mut self.rx, mpsc::channel(1).1));
        if let Some(writer) = self.writer.take() {
            // Bounded so a stuck peer cannot hold the caller forever.
            if tokio::time::timeout(std::time::Duration::from_secs(10), writer)
                .await
                .is_err()
            {
                tracing::debug!("link writer did not finish in time");
            }
        }
        // `Drop` aborts the reader.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (
        FramedConn<tokio::io::DuplexStream>,
        FramedConn<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (FramedConn::new(a), FramedConn::new(b))
    }

    /// A stream that reports when it is dropped, that is when the last holder of
    /// the connection (reader half, writer half) has let go of it.
    struct Probe {
        inner: tokio::io::DuplexStream,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl AsyncRead for Probe {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Probe {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// CF-19: dropping a link ends its reader task. The peer keeps its end open
    /// and never writes, so the reader would sit on the connection for ever;
    /// only the abort lets go of it.
    #[tokio::test]
    async fn dropping_a_link_aborts_its_reader() {
        let (a, _silent_peer) = tokio::io::duplex(64 * 1024);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let link = Link::new(FramedConn::new(Probe {
            inner: a,
            dropped: dropped.clone(),
        }));
        // Let the reader start and block on the silent peer.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!dropped.load(Ordering::SeqCst));
        drop(link);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the reader of a dropped link still holds the connection"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn closing_a_link_flushes_and_releases_the_connection() {
        let (a, mut b) = pair();
        let link = Link::new(a);
        link.send(Message::Started).await.unwrap();
        link.close().await;
        assert!(matches!(b.recv().await, Ok(Some(Message::Started))));
        assert!(matches!(b.recv().await, Ok(None)), "EOF after the flush");
    }

    #[tokio::test]
    async fn silent_connection_is_closed_after_the_idle_limit() {
        let (a, _b) = pair();
        let mut link = Link::new_with_idle(a, Some(Duration::from_millis(150)));
        let r = tokio::time::timeout(Duration::from_secs(5), link.recv())
            .await
            .expect("watchdog never fired");
        assert!(r.is_none(), "idle link must end");
    }

    #[tokio::test]
    async fn traffic_in_either_direction_keeps_the_link_alive_and_idle_can_be_lifted() {
        let (a, mut b) = pair();
        let mut link = Link::new_with_idle(a, Some(Duration::from_millis(300)));
        for _ in 0..6 {
            b.send(&Message::Started).await.unwrap();
            assert!(matches!(link.recv().await, Some(Message::Started)));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Sending counts as activity too.
        for _ in 0..6 {
            link.send(Message::Cancel).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), link.recv())
                .await
                .is_err(),
            "still open"
        );
        link.set_idle(None);
        assert!(
            tokio::time::timeout(Duration::from_millis(800), link.recv())
                .await
                .is_err(),
            "no limit: stays open"
        );
    }
}
