use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, RwLock},
};

use futures::{StreamExt, future};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use crate::{log_msg::LogMsg, stream_lines::LinesStreamExt};

/// Read a positive `usize` tunable from the environment, falling back to
/// `default` when unset, unparseable, or zero.
fn env_usize(name: &str, default: usize) -> usize {
    evaluate_usize_override(name, std::env::var(name).ok(), default)
}

/// Resolve a `usize` tunable without reading the environment, so the fallback
/// and warning behaviour is testable. Mirrors [`crate::env::evaluate_disable_flag`].
///
/// Zero is rejected rather than honoured because both call sites would
/// misbehave: a zero-byte history evicts every message it is handed, and
/// `broadcast::channel(0)` panics. A caller asking for zero has almost
/// certainly made a mistake, so the default is used and the surprise reported.
pub(crate) fn evaluate_usize_override(name: &str, value: Option<String>, default: usize) -> usize {
    let Some(raw) = value else {
        return default;
    };

    match raw.trim().parse::<usize>() {
        Ok(parsed) if parsed > 0 => parsed,
        _ => {
            tracing::warn!(
                variable = name,
                value = %raw,
                default,
                "expected a positive integer; falling back to the default"
            );
            default
        }
    }
}

/// Per-execution in-memory log history, retained so a dashboard client that
/// connects (or reconnects) mid-run gets scrollback.
///
/// This cost is paid PER RUNNING EXECUTION -- the server holds one `MsgStore`
/// per execution process in a map -- so peak usage is roughly this value times
/// the number of concurrent agents, not a single global budget.
///
/// Lowered from ~100 MB to 16 MB: on a host running 22 concurrent agents the
/// old value reserved ~2.2 GB for scrollback alone, and resident memory was
/// observed at 14.5 GB before an operator restarted the server. 16 MB is still
/// far more than a typical run emits.
///
/// Override with `VK_MSG_HISTORY_BYTES` (bytes).
static HISTORY_BYTES: LazyLock<usize> =
    LazyLock::new(|| env_usize("VK_MSG_HISTORY_BYTES", 16 * 1024 * 1024));

/// Capacity, in messages, of the live broadcast ring for each execution.
///
/// Tokio allocates this ring eagerly, so it costs memory per execution even
/// while idle. It also bounds how far a subscriber may fall behind: one that
/// lags by more than this many messages gets `Lagged` and permanently misses
/// them, so this trades memory against tolerance for a slow client.
///
/// Lowered from 100_000 to 8_192, which is still thousands of lines of burst
/// headroom for a websocket consumer that is keeping up.
///
/// Override with `VK_MSG_BROADCAST_CAPACITY` (messages).
static BROADCAST_CAPACITY: LazyLock<usize> =
    LazyLock::new(|| env_usize("VK_MSG_BROADCAST_CAPACITY", 8192));

#[derive(Clone)]
struct StoredMsg {
    msg: LogMsg,
    bytes: usize,
}

struct Inner {
    history: VecDeque<StoredMsg>,
    total_bytes: usize,
}

pub struct MsgStore {
    inner: RwLock<Inner>,
    sender: broadcast::Sender<LogMsg>,
}

impl Default for MsgStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MsgStore {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(*BROADCAST_CAPACITY);
        Self {
            inner: RwLock::new(Inner {
                history: VecDeque::with_capacity(32),
                total_bytes: 0,
            }),
            sender,
        }
    }

    pub fn push(&self, msg: LogMsg) {
        let bytes = msg.approx_bytes();

        let mut inner = self.inner.write().unwrap();
        while inner.total_bytes.saturating_add(bytes) > *HISTORY_BYTES {
            if let Some(front) = inner.history.pop_front() {
                inner.total_bytes = inner.total_bytes.saturating_sub(front.bytes);
            } else {
                break;
            }
        }
        inner.history.push_back(StoredMsg { msg, bytes });
        inner.total_bytes = inner.total_bytes.saturating_add(bytes);
        let message = inner
            .history
            .back()
            .expect("message was just stored")
            .msg
            .clone();
        let _ = self.sender.send(message); // live listeners
    }

    // Convenience
    pub fn push_stdout<S: Into<String>>(&self, s: S) {
        self.push(LogMsg::Stdout(s.into()));
    }

    pub fn push_patch(&self, patch: json_patch::Patch) {
        self.push(LogMsg::JsonPatch(patch));
    }

    pub fn push_session_id(&self, session_id: String) {
        self.push(LogMsg::SessionId(session_id));
    }

    pub fn push_message_id(&self, id: String) {
        self.push(LogMsg::MessageId(id));
    }

    pub fn push_native_uuid(&self, uuid: String) {
        self.push(LogMsg::NativeUuid(uuid));
    }

    pub fn push_finished(&self) {
        self.push(LogMsg::Finished);
    }

    pub fn get_receiver(&self) -> broadcast::Receiver<LogMsg> {
        self.sender.subscribe()
    }

    pub fn get_history(&self) -> Vec<LogMsg> {
        self.inner
            .read()
            .unwrap()
            .history
            .iter()
            .map(|s| s.msg.clone())
            .collect()
    }

    /// History then live, as `LogMsg`.
    pub fn history_plus_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<LogMsg, std::io::Error>> {
        // `push` records history and broadcasts while holding the write lock.
        // Subscribe and copy history under the matching read lock so an event
        // cannot fall into the gap between those two sources.
        let inner = self.inner.read().unwrap();
        let rx = self.sender.subscribe();
        let history = inner
            .history
            .iter()
            .map(|s| s.msg.clone())
            .collect::<Vec<_>>();
        drop(inner);

        let hist = futures::stream::iter(history.into_iter().map(Ok::<_, std::io::Error>));
        let live = BroadcastStream::new(rx).filter_map(|res| async move {
            match res {
                Ok(msg) => Some(Ok(msg)),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::error!(
                        skipped = n,
                        "MsgStore broadcast lagged. {n} messages dropped for this subscriber"
                    );
                    None
                }
            }
        });

        Box::pin(hist.chain(live))
    }

    pub fn stdout_chunked_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<String, std::io::Error>> {
        self.history_plus_stream()
            .take_while(|res| future::ready(!matches!(res, Ok(LogMsg::Finished))))
            .filter_map(|res| async move {
                match res {
                    Ok(LogMsg::Stdout(s)) => Some(Ok(s)),
                    _ => None,
                }
            })
            .boxed()
    }

    pub fn stdout_lines_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, std::io::Result<String>> {
        self.stdout_chunked_stream().lines()
    }

    pub fn stderr_chunked_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<String, std::io::Error>> {
        self.history_plus_stream()
            .take_while(|res| future::ready(!matches!(res, Ok(LogMsg::Finished))))
            .filter_map(|res| async move {
                match res {
                    Ok(LogMsg::Stderr(s)) => Some(Ok(s)),
                    _ => None,
                }
            })
            .boxed()
    }

    /// Forward a stream of typed log messages into this store.
    pub fn spawn_forwarder<S, E>(self: Arc<Self>, stream: S) -> JoinHandle<()>
    where
        S: futures::Stream<Item = Result<LogMsg, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        tokio::spawn(async move {
            tokio::pin!(stream);

            while let Some(next) = stream.next().await {
                match next {
                    Ok(msg) => self.push(msg),
                    Err(e) => self.push(LogMsg::Stderr(format!("stream error: {e}"))),
                }
            }
        })
    }
}

#[cfg(test)]
mod buffer_tunable_tests {
    use super::evaluate_usize_override;

    const DEFAULT: usize = 16 * 1024 * 1024;

    #[test]
    fn unset_uses_the_default() {
        assert_eq!(evaluate_usize_override("VK_X", None, DEFAULT), DEFAULT);
    }

    #[test]
    fn a_positive_value_overrides() {
        assert_eq!(
            evaluate_usize_override("VK_X", Some("4096".to_string()), DEFAULT),
            4096
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            evaluate_usize_override("VK_X", Some("  4096\n".to_string()), DEFAULT),
            4096
        );
    }

    #[test]
    fn zero_falls_back_rather_than_disabling_the_buffer() {
        // broadcast::channel(0) panics and a zero-byte history evicts
        // everything, so zero must never reach the call sites.
        assert_eq!(
            evaluate_usize_override("VK_X", Some("0".to_string()), DEFAULT),
            DEFAULT
        );
    }

    #[test]
    fn unparseable_and_negative_values_fall_back() {
        for raw in ["", "not-a-number", "-1", "12.5"] {
            assert_eq!(
                evaluate_usize_override("VK_X", Some(raw.to_string()), DEFAULT),
                DEFAULT,
                "expected {raw:?} to fall back"
            );
        }
    }
}
