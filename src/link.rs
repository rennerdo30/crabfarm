//! A framed connection split into a reader task and a writer task, so the
//! owner can `select!` on incoming messages without dropping a half-read frame
//! (`FramedConn::recv` is not cancel-safe).
//!
//! Dropping a `Link` drops its sender; the writer then drains its queue and
//! closes the write half, the peer sees EOF, and the reader task ends with the
//! connection. `close` does the same but waits for the queue to be flushed.

use crate::proto::Message;
use crate::transport::FramedConn;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Frames queued per direction before the sender waits (backpressure).
const QUEUE: usize = 64;

pub struct Link {
    pub tx: mpsc::Sender<Message>,
    pub rx: mpsc::Receiver<Message>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

impl Link {
    pub fn new<S>(conn: FramedConn<S>) -> Link
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (rd, wr) = tokio::io::split(conn.stream);
        let mut rd = FramedConn::new(rd);
        let mut wr = FramedConn::new(wr);
        let (tx, mut out_rx) = mpsc::channel::<Message>(QUEUE);
        let (in_tx, rx) = mpsc::channel::<Message>(QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if let Err(e) = wr.send(&m).await {
                    tracing::debug!("link write failed: {e:#}");
                    break;
                }
            }
            // Closing the write half lets the peer see a clean EOF.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr.stream).await;
        });
        let reader = tokio::spawn(async move {
            loop {
                match rd.recv().await {
                    Ok(Some(m)) => {
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
        Link { tx, rx, writer, reader }
    }

    /// Queue a frame. Fails once the writer task has stopped (connection gone).
    pub async fn send(&self, m: Message) -> anyhow::Result<()> {
        self.tx.send(m).await.map_err(|_| anyhow::anyhow!("connection closed"))
    }

    /// Next frame; `None` when the connection ended.
    pub async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await
    }

    /// Flush everything queued, then close.
    pub async fn close(self) {
        let Link { tx, rx, writer, reader } = self;
        drop(tx);
        drop(rx);
        // Bounded so a stuck peer cannot hold the caller forever.
        if tokio::time::timeout(std::time::Duration::from_secs(10), writer).await.is_err() {
            tracing::debug!("link writer did not finish in time");
        }
        reader.abort();
    }
}
