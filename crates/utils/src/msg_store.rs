use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, RwLock},
};

use futures::{StreamExt, future};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use crate::{env::env_usize, log_msg::LogMsg, stream_lines::LinesStreamExt};

/// Byte budget for the in-memory log history each [`MsgStore`] retains, so a
/// dashboard client that connects (or reconnects) mid-run gets scrollback.
///
/// This is charged PER STORE, and there is more than one kind of store: the
/// server creates one per running execution (`services::container`), plus a
/// single process-lifetime events store fed by the DB change hook
/// (`local_deployment`), plus short-lived stores used to replay a finished
/// execution. Peak usage is therefore roughly this value times the number of
/// concurrent agents, plus a constant.
///
/// Lowered from ~100 MB to 16 MB: on a host running 22 concurrent agents the
/// old value allowed scrollback to reach GBs on its own (the deque grows on
/// demand — this is a ceiling, not a reservation) and resident memory reached
/// 14.5 GB before an operator restarted the server. Note the accounting is
/// approximate — `LogMsg::approx_bytes` measures a patch by its serialized
/// length, which understates its real heap cost — so treat this as a budget
/// knob, not an exact resident-memory guarantee.
///
/// Override with `BC_MSG_HISTORY_BYTES` (bytes).
static HISTORY_BYTES: LazyLock<usize> =
    LazyLock::new(|| env_usize("BC_MSG_HISTORY_BYTES", 16 * 1024 * 1024));

/// Capacity, in messages, of the live broadcast ring each [`MsgStore`] creates.
///
/// Tokio rounds this up to a power of two and allocates the ring eagerly, and
/// each occupied slot retains a clone of the message until every receiver has
/// consumed it — so this costs memory per store even while idle.
///
/// It also bounds how far a subscriber may fall behind: one that lags past
/// capacity receives `Lagged` and PERMANENTLY misses those messages. What
/// happens next depends on the consumer, and the two differ:
/// `history_plus_stream` logs `MsgStore broadcast lagged` at error level and
/// then drops the item without surfacing an error downstream, so the gap is
/// recorded but invisible to the reader; the events SSE streams in
/// `services::events::streams` discard the error with no log at all.
///
/// The binding constraint is NOT the dashboard — it is
/// `spawn_stream_raw_logs_to_storage`, which consumes this same stream to
/// write the durable per-execution JSONL. A lagging writer therefore leaves a
/// gap in the on-disk record of what an agent did. That path does at least go
/// through `history_plus_stream`, so such a gap is greppable.
///
/// Lowered from 100_000 (131_072 after rounding) to 32_768. That is a 4x cut
/// rather than the 16x an aggressive value would give, deliberately keeping
/// headroom for the persistence path until it stops riding a lossy broadcast.
///
/// Override with `BC_MSG_BROADCAST_CAPACITY` (messages; rounded up to a power
/// of two by tokio).
// TODO(bc-msg-buffers): retire the headroom this value is holding open —
// give the durable-log writer a lossless transport instead of a broadcast
// subscription, and add `MsgStore::with_limits` so the replay store in
// `services::container` is not bounded by a knob tuned for live memory.
static BROADCAST_CAPACITY: LazyLock<usize> =
    LazyLock::new(|| env_usize("BC_MSG_BROADCAST_CAPACITY", 32 * 1024));

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
