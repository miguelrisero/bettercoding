use std::{
    collections::HashMap,
    io::{IsTerminal, Write},
    sync::Arc,
};

use anyhow::{Context, Result};
use db::{
    DBService,
    models::{
        coding_agent_turn::CodingAgentTurn, execution_native_link::ExecutionNativeLink,
        execution_process::ExecutionProcess, execution_process_logs::ExecutionProcessLogs,
    },
};
use futures::{StreamExt, TryStreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use sqlx::SqlitePool;
use tokio::{io::AsyncWriteExt, sync::RwLock, task::JoinHandle};
use utils::{
    assets::try_prod_asset_dir_path,
    execution_logs::{
        ExecutionLogWriter, process_log_file_path, process_log_file_path_in_root,
        read_execution_log_file,
    },
    log_msg::LogMsg,
    msg_store::MsgStore,
};
use uuid::Uuid;

use crate::services::claude_transcript_ingest::{
    NativeLinkPersisted, notify_native_link_persisted,
};

const NATIVE_LINK_INSERT_ATTEMPTS: usize = 5;
const NATIVE_LINK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

pub async fn migrate_execution_logs_to_files() -> Result<()> {
    let pool = DBService::new_migration_pool()
        .await
        .map_err(|e| anyhow::anyhow!("Migration DB pool error: {}", e))?;

    if !ExecutionProcessLogs::has_any(&pool).await? {
        return Ok(());
    }

    let is_tty = std::io::stderr().is_terminal();
    if is_tty {
        let _ = writeln!(
            std::io::stderr(),
            "Performing one time database migration to move logs from SQLite to flat file to improve performance, data remains local, may take a few minutes, please don't exit while this process is running..."
        );
    }

    let pb = if is_tty {
        Some(new_spinner("Migrating"))
    } else {
        None
    };

    let total_processes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let count_task = {
        let pool = pool.clone();
        let pb = pb.clone();
        let total_processes = total_processes.clone();
        tokio::spawn(async move {
            if let Ok(count) = ExecutionProcessLogs::count_distinct_processes(&pool).await {
                total_processes.store(count as usize, std::sync::atomic::Ordering::Relaxed);
                if let Some(pb) = pb {
                    pb.set_length(count as u64);
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("{bar:36.yellow} {percent:>3}% {msg:<12.dim}")
                            .unwrap_or_else(|_| ProgressStyle::default_bar())
                            .progress_chars("■⬝"),
                    );
                }
            }
        })
    };

    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    ExecutionProcessLogs::stream_distinct_processes(&pool)
        .map_err(anyhow::Error::from)
        .map(|res| {
            let pool = pool.clone();
            let pb = pb.clone();
            let completed = completed.clone();
            let total_processes = total_processes.clone();
            async move {
                let p = res?;

                let path = process_log_file_path(p.session_id, p.execution_id);
                if path.exists() {
                    if let Some(pb) = &pb {
                        pb.inc(1);
                    }
                    return Ok::<(), anyhow::Error>(());
                }

                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let temp_path = path.with_extension("jsonl.tmp");
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&temp_path)
                    .await?;

                let mut logs_stream =
                    ExecutionProcessLogs::stream_log_lines_by_execution_id(&pool, &p.execution_id);
                let mut has_logs = false;
                while let Some(log_res) = logs_stream.next().await {
                    let log = log_res?;
                    has_logs = true;
                    let mut line = log;
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    file.write_all(line.as_bytes()).await?;
                }

                if !has_logs {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    if let Some(pb) = &pb {
                        pb.inc(1);
                    }
                    return Ok::<(), anyhow::Error>(());
                }

                file.sync_all().await?;
                tokio::fs::rename(temp_path, path).await?;

                let c = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

                if let Some(pb) = &pb {
                    pb.inc(1);
                } else if c.is_multiple_of(100) {
                    let t = total_processes.load(std::sync::atomic::Ordering::Relaxed);
                    let _ = writeln!(
                        std::io::stderr(),
                        "sqlite-migration:{}",
                        if t > 0 {
                            (c * 100 / t).to_string()
                        } else {
                            "?".to_string()
                        }
                    );
                }

                Ok::<(), anyhow::Error>(())
            }
        })
        .buffer_unordered(64)
        .try_collect::<Vec<_>>()
        .await?;

    let _ = count_task.await;

    if let Some(pb) = pb {
        pb.finish_and_clear();
    } else {
        let _ = writeln!(std::io::stderr(), "sqlite-migration:done");
    }

    let vacuum_pb = if is_tty {
        Some(new_spinner("Compacting"))
    } else {
        let _ = writeln!(std::io::stderr(), "Compacting database...");
        None
    };

    ExecutionProcessLogs::delete_all(&pool).await?;
    sqlx::query("VACUUM").execute(&pool).await?;

    if let Some(pb) = vacuum_pb {
        pb.finish_and_clear();
    }

    let _ = writeln!(std::io::stderr(), "Database migration complete.");

    pool.close().await;

    Ok(())
}

pub async fn remove_session_process_logs(session_id: Uuid) -> Result<()> {
    let dir = utils::execution_logs::process_logs_session_dir(session_id);
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("remove session process logs at {}", dir.display()))
        }
    }
}

pub async fn load_raw_log_messages(pool: &SqlitePool, execution_id: Uuid) -> Option<Vec<LogMsg>> {
    if let Some(jsonl) = read_execution_logs_for_execution(pool, execution_id)
        .await
        .inspect_err(|e| {
            tracing::warn!(
                "Failed to read execution log file for execution {}: {:#}",
                execution_id,
                e
            );
        })
        .ok()
        .flatten()
    {
        let messages = utils::execution_logs::parse_log_jsonl_lossy(execution_id, &jsonl);
        if !messages.is_empty() {
            return Some(messages);
        }
    }

    let db_log_records = match ExecutionProcessLogs::find_by_execution_id(pool, execution_id).await
    {
        Ok(records) if !records.is_empty() => records,
        Ok(_) => return None,
        Err(e) => {
            tracing::error!(
                "Failed to fetch DB logs for execution {}: {}",
                execution_id,
                e
            );
            return None;
        }
    };

    match ExecutionProcessLogs::parse_logs(&db_log_records) {
        Ok(msgs) => Some(msgs),
        Err(e) => {
            tracing::error!(
                "Failed to parse DB logs for execution {}: {}",
                execution_id,
                e
            );
            None
        }
    }
}

pub async fn append_log_message(session_id: Uuid, execution_id: Uuid, msg: &LogMsg) -> Result<()> {
    let mut log_writer = ExecutionLogWriter::new_for_execution(session_id, execution_id)
        .await
        .with_context(|| format!("create log writer for execution {}", execution_id))?;
    let json_line = serde_json::to_string(msg)
        .with_context(|| format!("serialize log message for execution {}", execution_id))?;
    let mut json_line_with_newline = json_line;
    json_line_with_newline.push('\n');
    log_writer
        .append_jsonl_line(&json_line_with_newline)
        .await
        .with_context(|| format!("append log message for execution {}", execution_id))?;
    Ok(())
}

pub fn spawn_stream_raw_logs_to_storage(
    msg_stores: Arc<RwLock<HashMap<Uuid, Arc<MsgStore>>>>,
    db: DBService,
    execution_id: Uuid,
    session_id: Uuid,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut log_writer =
            match ExecutionLogWriter::new_for_execution(session_id, execution_id).await {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(
                        "Failed to create log file writer for execution {}: {}",
                        execution_id,
                        e
                    );
                    return;
                }
            };

        let store = {
            let map = msg_stores.read().await;
            map.get(&execution_id).cloned()
        };

        if let Some(store) = store {
            let output_store = store.clone();
            let persist_metadata = persist_execution_metadata(store, db, execution_id);
            let persist_output = async move {
                let mut stream = output_store.history_plus_stream();
                drop(output_store);
                while let Some(Ok(msg)) = stream.next().await {
                    match &msg {
                        LogMsg::Stdout(_) | LogMsg::Stderr(_) => {
                            match serde_json::to_string(&msg) {
                                Ok(jsonl_line) => {
                                    let mut jsonl_line_with_newline = jsonl_line;
                                    jsonl_line_with_newline.push('\n');

                                    if let Err(e) =
                                        log_writer.append_jsonl_line(&jsonl_line_with_newline).await
                                    {
                                        tracing::error!(
                                            "Failed to append log line for execution {}: {}",
                                            execution_id,
                                            e
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to serialize log message for execution {}: {}",
                                        execution_id,
                                        e
                                    );
                                }
                            }
                        }
                        LogMsg::Finished => {
                            break;
                        }
                        LogMsg::SessionId(_)
                        | LogMsg::MessageId(_)
                        | LogMsg::NativeUuid(_)
                        | LogMsg::JsonPatch(_)
                        | LogMsg::Ready => continue,
                    }
                }
            };
            tokio::join!(persist_output, persist_metadata);
        }
    })
}

async fn persist_execution_metadata(store: Arc<MsgStore>, db: DBService, execution_id: Uuid) {
    let mut stream = store.history_plus_stream();
    // The stream's receiver is sufficient. Releasing this Arc lets the
    // channel close once normalizers and the MsgStore registry are done, at
    // which point every event after Finished has been drained.
    drop(store);
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            LogMsg::SessionId(agent_session_id) => {
                if let Err(error) = CodingAgentTurn::update_agent_session_id(
                    &db.pool,
                    execution_id,
                    &agent_session_id,
                )
                .await
                {
                    tracing::error!(
                        %error,
                        %agent_session_id,
                        %execution_id,
                        "failed to persist native session id"
                    );
                }
            }
            LogMsg::MessageId(agent_message_id) => {
                if let Err(error) = CodingAgentTurn::update_agent_message_id(
                    &db.pool,
                    execution_id,
                    &agent_message_id,
                )
                .await
                {
                    tracing::error!(
                        %error,
                        %agent_message_id,
                        %execution_id,
                        "failed to persist native message id"
                    );
                }
            }
            LogMsg::NativeUuid(native_uuid) => {
                persist_native_link_with_retry(&db, execution_id, &native_uuid).await;
            }
            LogMsg::Finished
            | LogMsg::Stdout(_)
            | LogMsg::Stderr(_)
            | LogMsg::JsonPatch(_)
            | LogMsg::Ready => {}
        }
    }
}

pub(crate) async fn persist_native_link_with_retry(
    db: &DBService,
    execution_id: Uuid,
    native_uuid: &str,
) -> bool {
    for attempt in 1..=NATIVE_LINK_INSERT_ATTEMPTS {
        match ExecutionNativeLink::insert(&db.pool, execution_id, native_uuid).await {
            Ok(_) => {
                notify_native_link_persisted(NativeLinkPersisted {
                    execution_process_id: execution_id,
                    native_uuid: native_uuid.to_string(),
                });
                return true;
            }
            Err(error) if attempt < NATIVE_LINK_INSERT_ATTEMPTS => {
                tracing::warn!(
                    %error,
                    %execution_id,
                    %native_uuid,
                    attempt,
                    "retrying native UUID persistence"
                );
                tokio::time::sleep(NATIVE_LINK_RETRY_DELAY).await;
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    %execution_id,
                    %native_uuid,
                    attempts = attempt,
                    "failed to persist native UUID after retries"
                );
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use db::models::{
        session::{CreateSession, Session},
        workspace::{CreateWorkspace, Workspace},
    };
    use executors::{
        actions::{
            ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
        },
        executors::BaseCodingAgent,
        profile::ExecutorConfig,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn execution_fixture() -> (DBService, Uuid) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        db::run_migrations_for_tests(&pool).await.unwrap();
        let db = DBService { pool };
        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &db.pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: None,
            },
            workspace_id,
        )
        .await
        .unwrap();
        let session = Session::create(
            &db.pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            Uuid::new_v4(),
            workspace_id,
        )
        .await
        .unwrap();
        let execution_id = Uuid::new_v4();
        ExecutionProcess::create(
            &db.pool,
            &db::models::execution_process::CreateExecutionProcess {
                session_id: session.id,
                executor_action: ExecutorAction::new(
                    ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                        prompt: "late uuid".to_string(),
                        executor_config: ExecutorConfig::new(BaseCodingAgent::ClaudeCode),
                        working_dir: None,
                    }),
                    None,
                ),
                run_reason: db::models::execution_process::ExecutionProcessRunReason::CodingAgent,
            },
            execution_id,
            &[],
        )
        .await
        .unwrap();
        (db, execution_id)
    }

    #[tokio::test]
    async fn metadata_drain_persists_uuid_emitted_after_finished() {
        let (db, execution_id) = execution_fixture().await;
        let store = Arc::new(MsgStore::new());
        let task = tokio::spawn(persist_execution_metadata(
            store.clone(),
            db.clone(),
            execution_id,
        ));
        tokio::task::yield_now().await;

        store.push_finished();
        store.push_native_uuid("late-after-finished".to_string());
        drop(store);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("metadata consumer did not drain to store closure")
            .unwrap();

        assert_eq!(
            ExecutionNativeLink::find_execution_id(&db.pool, "late-after-finished")
                .await
                .unwrap(),
            Some(execution_id)
        );
    }
}

async fn read_execution_logs_for_execution(
    pool: &SqlitePool,
    execution_id: Uuid,
) -> Result<Option<String>> {
    let session_id = if let Some(process) = ExecutionProcess::find_by_id(pool, execution_id).await?
    {
        process.session_id
    } else {
        return Ok(None);
    };
    let path = process_log_file_path(session_id, execution_id);

    match tokio::fs::metadata(&path).await {
        Ok(_) => Ok(Some(read_execution_log_file(&path).await.with_context(
            || format!("read execution log file for execution {execution_id}"),
        )?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if cfg!(debug_assertions) {
                // Convenience for local development with a clone of a prod db. Read only access to prod logs.
                if let Ok(prod_asset_dir) = try_prod_asset_dir_path() {
                    let prod_path =
                        process_log_file_path_in_root(&prod_asset_dir, session_id, execution_id);
                    match read_execution_log_file(&prod_path).await {
                        Ok(contents) => return Ok(Some(contents)),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(err).with_context(|| {
                                format!(
                                    "read execution log file for execution {execution_id} from {}",
                                    prod_path.display()
                                )
                            });
                        }
                    }
                }
            }
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "check execution log file exists for execution {execution_id} at {}",
                path.display()
            )
        }),
    }
}

fn new_spinner(message: &'static str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.yellow} {msg:<12.dim}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
    );
    pb.set_message(message);
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}
