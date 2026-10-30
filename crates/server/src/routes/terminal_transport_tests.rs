//! Exercise the production terminal loop over bounded, in-memory WebSocket I/O.
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::extract::ws::Message;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use relay_protocol::RelayTransportMessage;
use tokio::{io::DuplexStream, sync::mpsc, task::JoinHandle};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, protocol::Role},
};
use uuid::Uuid;

use super::run_terminal_transport;

struct TestSocket(WebSocketStream<DuplexStream>);

impl Stream for TestSocket {
    type Item = anyhow::Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx).map(|item| {
            item.map(|result| {
                result
                    .map_err(anyhow::Error::from)
                    .and_then(|message| Message::try_from_frame(message.into_frame()))
            })
        })
    }
}

impl Sink<Message> for TestSocket {
    type Error = anyhow::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0)
            .poll_ready(cx)
            .map_err(anyhow::Error::from)
    }

    fn start_send(mut self: Pin<&mut Self>, message: Message) -> anyhow::Result<()> {
        Pin::new(&mut self.0)
            .start_send(tungstenite::Message::try_from_frame(message.into_frame())?)
            .map_err(anyhow::Error::from)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(anyhow::Error::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(anyhow::Error::from)
    }
}

struct Connection {
    id: Uuid,
    peer: WebSocketStream<DuplexStream>,
    output: mpsc::Sender<Vec<u8>>,
    closed: mpsc::UnboundedReceiver<Uuid>,
    task: JoinHandle<()>,
}

async fn connection() -> Connection {
    let (server, peer) = tokio::io::duplex(1024);
    let socket = TestSocket(WebSocketStream::from_raw_socket(server, Role::Server, None).await);
    let peer = WebSocketStream::from_raw_socket(peer, Role::Client, None).await;
    let (output, output_rx) = mpsc::channel(8);
    let (close_tx, closed) = mpsc::unbounded_channel();
    let id = Uuid::new_v4();
    let task = tokio::spawn(run_terminal_transport(
        socket,
        output_rx,
        |_| async {},
        move || async move {
            close_tx.send(id).unwrap();
        },
    ));
    Connection {
        id,
        peer,
        output,
        closed,
        task,
    }
}

async fn elapse(seconds: u64) {
    for _ in 0..seconds {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn silent_peer_releases_only_its_connection_within_sixty_seconds() {
    let mut stale = connection().await;
    let mut other = connection().await;
    let responder = tokio::spawn(async move {
        while let Some(message) = other.peer.next().await {
            if matches!(message.unwrap(), tungstenite::Message::Ping(_)) {
                other.peer.flush().await.unwrap();
            }
        }
    });
    elapse(60).await;
    assert_eq!(stale.closed.try_recv().ok(), Some(stale.id));
    assert!(stale.output.is_closed());
    assert!(other.closed.try_recv().is_err());
    assert!(!other.task.is_finished());
    stale.task.await.unwrap();
    responder.abort();
    other.task.abort();
}

#[tokio::test(start_paused = true)]
async fn responsive_idle_peer_survives_multiple_heartbeat_windows() {
    let mut connection = connection().await;
    let pings = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_pings = pings.clone();
    let responder = tokio::spawn(async move {
        while let Some(message) = connection.peer.next().await {
            match message.unwrap() {
                tungstenite::Message::Ping(_) => {
                    observed_pings.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Tungstenite queues the protocol Pong when it reads Ping, as browsers do.
                    connection.peer.flush().await.unwrap();
                }
                tungstenite::Message::Close(_) => break,
                _ => {}
            }
        }
    });
    elapse(90).await;
    assert!(pings.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    assert!(connection.closed.try_recv().is_err());
    assert!(!connection.task.is_finished());
    responder.abort();
    connection.task.abort();
}

#[tokio::test(start_paused = true)]
async fn close_and_transport_error_each_run_exact_cleanup_once() {
    for graceful in [true, false] {
        let mut connection = connection().await;
        if graceful {
            connection
                .peer
                .send(tungstenite::Message::Close(None))
                .await
                .unwrap();
        }
        drop(connection.peer);
        connection.task.await.unwrap();
        assert_eq!(connection.closed.recv().await, Some(connection.id));
        assert_eq!(connection.closed.recv().await, None);
        assert!(connection.output.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_output_send_releases_connection_within_ten_seconds() {
    let mut connection = connection().await;
    connection.output.send(vec![b'x'; 64 * 1024]).await.unwrap();
    elapse(10).await;
    assert_eq!(connection.closed.try_recv().ok(), Some(connection.id));
    assert!(connection.output.is_closed());
    connection.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn continuous_output_cannot_starve_silent_peer_expiry() {
    let mut connection = connection().await;
    let output = connection.output.clone();
    let producer = tokio::spawn(async move { while output.send(vec![b'x'; 16]).await.is_ok() {} });
    // Drain raw bytes without decoding Ping: the peer accepts output but never answers.
    let drain = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut bytes = [0; 4096];
        while connection
            .peer
            .get_mut()
            .read(&mut bytes)
            .await
            .unwrap_or(0)
            != 0
        {}
    });
    elapse(60).await;
    assert_eq!(connection.closed.try_recv().ok(), Some(connection.id));
    connection.task.await.unwrap();
    producer.await.unwrap();
    drain.await.unwrap();
}
