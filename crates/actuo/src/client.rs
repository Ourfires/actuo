//! A protocol client over actuo's Unix socket: calls, `event` notifications
//! on a channel, and `run/subscribe`'s on another.

use actuo_protocol::errors::RpcError;
use actuo_protocol::events::Event;
use actuo_protocol::jsonrpc::{Id, Message};
use actuo_protocol::methods::{Initialize, InitializeParams, Method};
use actuo_protocol::runs::{CheckRun, Run};
use actuo_protocol::types::{ActuoInfo, ClientInfo};
use actuo_protocol::{
    CHECK_STATE_NOTIFICATION, EVENT_NOTIFICATION, PROTOCOL_VERSION, RUN_STATE_NOTIFICATION,
};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A notification after `run/subscribe`: a run whose state or step changed, or a
/// check run as it starts, after each step and when it ends.
pub enum RunNotice {
    Run(Run),
    Check(CheckRun),
}

pub struct Client {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    next_id: AtomicI64,
    pending: Pending,
    run_notices: Mutex<Option<mpsc::UnboundedReceiver<RunNotice>>>,
    /// What actuo said about itself in `initialize`.
    pub actuo: Option<ActuoInfo>,
}

impl Client {
    /// Connects and does the `initialize` handshake. Events arrive on the receiver.
    pub async fn connect(socket: &Path) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let stream = UnixStream::connect(socket).await.with_context(|| {
            format!(
                "actuo isn't running (nothing answers at {}): start it with `actuo service install`, or `actuo serve` in the foreground",
                socket.display()
            )
        })?;
        let (reader, writer) = stream.into_split();
        let pending: Pending = Arc::default();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (runs_tx, runs_rx) = mpsc::unbounded_channel();
        let routes = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut msg) = serde_json::from_str::<Message>(&line) else {
                    continue;
                };
                let params = msg.params.take().unwrap_or(Value::Null);
                if msg.method.as_deref() == Some(EVENT_NOTIFICATION) {
                    if let Ok(event) = serde_json::from_value(params) {
                        let _ = events_tx.send(event);
                    }
                } else if msg.method.as_deref() == Some(RUN_STATE_NOTIFICATION) {
                    if let Ok(run) = serde_json::from_value(params) {
                        let _ = runs_tx.send(RunNotice::Run(run));
                    }
                } else if msg.method.as_deref() == Some(CHECK_STATE_NOTIFICATION) {
                    if let Ok(check) = serde_json::from_value(params) {
                        let _ = runs_tx.send(RunNotice::Check(check));
                    }
                } else if let Some(Id::Number(id)) = msg.id
                    && let Some(tx) = routes.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(match msg.error {
                        Some(e) => Err(e),
                        None => Ok(msg.result.unwrap_or(Value::Null)),
                    });
                }
            }
            // Connection closed: pending calls fail.
            routes.lock().unwrap().clear();
        });
        let mut client = Self {
            writer: tokio::sync::Mutex::new(writer),
            next_id: AtomicI64::new(1),
            pending,
            run_notices: Mutex::new(Some(runs_rx)),
            actuo: None,
        };
        let init = client
            .call::<Initialize>(InitializeParams {
                protocol_version: PROTOCOL_VERSION,
                client: ClientInfo {
                    name: "actuo-cli".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            })
            .await?;
        client.actuo = Some(init.actuo);
        Ok((client, events_rx))
    }

    /// `run/subscribe`'s notifications; once per connection.
    pub fn run_notices(&self) -> mpsc::UnboundedReceiver<RunNotice> {
        self.run_notices
            .lock()
            .unwrap()
            .take()
            .expect("run notices are taken once")
    }

    pub async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result> {
        let value = self
            .raw_call(M::NAME, serde_json::to_value(params)?)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn raw_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&Message::request(Id::Number(id), method, params))
            .expect("request serializes");
        line.push(b'\n');
        let gone = || {
            RpcError::new(
                actuo_protocol::errors::ErrorKind::Internal,
                "actuo closed the connection",
            )
        };
        {
            let mut w = self.writer.lock().await;
            if w.write_all(&line).await.is_err() {
                return Err(gone());
            }
        }
        rx.await.map_err(|_| gone())?
    }
}
