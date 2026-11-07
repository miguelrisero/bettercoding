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

struct TestSocket<S>(S);

impl<S, E> Stream for TestSocket<S>
where
    S: Stream<Item = Result<tungstenite::Message, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    type Item = anyhow::Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx).map(|item| {
            item.map(|result| {
                result
                    .map_err(Into::into)
                    .and_then(|message| Message::try_from_frame(message.into_frame()))
            })
        })
    }
}

impl<S> Sink<Message> for TestSocket<S>
where
    S: Sink<tungstenite::Message> + Unpin,
    S::Error: Into<anyhow::Error>,
{
    type Error = anyhow::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0).poll_ready(cx).map_err(Into::into)
    }

    fn start_send(mut self: Pin<&mut Self>, message: Message) -> anyhow::Result<()> {
        Pin::new(&mut self.0)
            .start_send(tungstenite::Message::try_from_frame(message.into_frame())?)
            .map_err(Into::into)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Pin::new(&mut self.0).poll_close(cx).map_err(Into::into)
    }
}

struct Connection<Peer = WebSocketStream<DuplexStream>> {
    id: Uuid,
    peer: Peer,
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
        false,
        |_| async { Ok(()) },
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
            tokio::time::timeout(Duration::from_secs(1), &mut connection.task)
                .await
                .expect("Close must finish while the peer remains connected")
                .unwrap();
        } else {
            drop(connection.peer);
            tokio::time::timeout(Duration::from_secs(1), &mut connection.task)
                .await
                .expect("transport error must finish cleanup")
                .unwrap();
        }
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

#[tokio::test(start_paused = true)]
async fn wrong_pong_challenge_cannot_keep_a_connection_alive() {
    let mut connection = connection().await;
    let responder = tokio::spawn(async move {
        while let Some(message) = connection.peer.next().await {
            let Ok(message) = message else { break };
            if matches!(message, tungstenite::Message::Ping(_)) {
                // Replace the automatically queued matching Pong before flushing it.
                if connection
                    .peer
                    .send(tungstenite::Message::Pong(
                        b"foreign-attach".to_vec().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    elapse(60).await;
    assert_eq!(connection.closed.try_recv().ok(), Some(connection.id));
    connection.task.await.unwrap();
    responder.abort();
}

// Exercise the real authenticated transport, including signature and sequence verification.
async fn signed_connection() -> Connection<relay_ws::SignedTungsteniteSocket> {
    use relay_control::signing::{RelaySigningService, RequestSignature};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::MaybeTlsStream;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let (client, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let server = accepted.unwrap().0;
    let client = client.unwrap();
    let server_key = ed25519_dalek::SigningKey::from_bytes(&[31; 32]);
    let client_key = ed25519_dalek::SigningKey::from_bytes(&[47; 32]);
    let server_signing = RelaySigningService::new(server_key.clone());
    let client_signing = RelaySigningService::new(client_key.clone());
    let signature = RequestSignature {
        signing_session_id: Uuid::new_v4(),
        timestamp: 0,
        nonce: Uuid::new_v4(),
        signature_b64: String::new(),
    };
    server_signing
        .register_session(signature.signing_session_id, client_key.verifying_key())
        .await;
    client_signing
        .register_session(signature.signing_session_id, server_key.verifying_key())
        .await;
    let server =
        WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(server), Role::Server, None).await;
    let client =
        WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(client), Role::Client, None).await;
    let socket = TestSocket(
        relay_ws::signed_tungstenite_websocket(&server_signing, &signature, server)
            .await
            .unwrap(),
    );
    let peer = relay_ws::signed_tungstenite_websocket(&client_signing, &signature, client)
        .await
        .unwrap();
    let (output, output_rx) = mpsc::channel(8);
    let (close_tx, closed) = mpsc::unbounded_channel();
    let id = Uuid::new_v4();
    let task = tokio::spawn(run_terminal_transport(
        socket,
        output_rx,
        true,
        |_| async { Ok(()) },
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

#[tokio::test(start_paused = true)]
async fn legacy_signed_peer_survives_multiple_unanswered_rotating_probes() {
    let mut connection = signed_connection().await;
    let challenges = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received = challenges.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = connection.peer.next().await {
            if let tungstenite::Message::Ping(payload) = message {
                received.lock().unwrap().push(payload.to_vec());
            }
        }
    });
    elapse(90).await;
    assert!(connection.closed.try_recv().is_err());
    assert!(!connection.task.is_finished());
    let probes = challenges.lock().unwrap();
    assert!(probes.len() >= 2);
    assert!(probes.windows(2).all(|pair| pair[0] != pair[1]));
    reader.abort();
    connection.task.abort();
}

#[tokio::test(start_paused = true)]
async fn signed_matching_pong_enables_expiry_after_capable_idle_viewing() {
    let mut connection = signed_connection().await;
    let respond = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let responding = respond.clone();
    let replies = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let replied = replies.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = connection.peer.next().await {
            if let tungstenite::Message::Ping(payload) = message
                && responding.load(std::sync::atomic::Ordering::Relaxed)
            {
                if connection
                    .peer
                    .send(tungstenite::Message::Pong(payload))
                    .await
                    .is_err()
                {
                    break;
                }
                replied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    elapse(90).await;
    assert!(replies.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    assert!(connection.closed.try_recv().is_err());
    respond.store(false, std::sync::atomic::Ordering::Relaxed);
    elapse(60).await;
    assert_eq!(connection.closed.try_recv().ok(), Some(connection.id));
    connection.task.await.unwrap();
    reader.abort();
}

#[tokio::test(start_paused = true)]
async fn signed_wrong_challenge_never_enables_receive_expiry() {
    let mut connection = signed_connection().await;
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = connection.peer.next().await {
            if matches!(message, tungstenite::Message::Ping(_))
                && connection
                    .peer
                    .send(tungstenite::Message::Pong(
                        b"foreign-attach".to_vec().into(),
                    ))
                    .await
                    .is_err()
            {
                break;
            }
        }
    });
    elapse(90).await;
    assert!(connection.closed.try_recv().is_err());
    assert!(!connection.task.is_finished());
    reader.abort();
    connection.task.abort();
}

#[tokio::test]
async fn rejected_input_returns_explicit_error_and_keeps_transport_available() {
    for error in ["CLI control busy; resubmit", "Failed to write to PTY"] {
        let (server, peer) = tokio::io::duplex(1024);
        let socket = TestSocket(WebSocketStream::from_raw_socket(server, Role::Server, None).await);
        let mut peer = WebSocketStream::from_raw_socket(peer, Role::Client, None).await;
        let (_output, output_rx) = mpsc::channel(8);
        let task = tokio::spawn(run_terminal_transport(
            socket,
            output_rx,
            false,
            move |_| async move { Err(error.to_string()) },
            || async {},
        ));
        for _ in 0..2 {
            peer.send(tungstenite::Message::Text(
                r#"{"type":"input","data":"eA=="}"#.into(),
            ))
            .await
            .unwrap();
            let response = peer.next().await.unwrap().unwrap().into_text().unwrap();
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["type"], "input_rejected");
            assert_eq!(response["message"], error);
        }
        peer.send(tungstenite::Message::Close(None)).await.unwrap();
        task.await.unwrap();
    }
}
