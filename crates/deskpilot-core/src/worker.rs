//! Elevated worker: a second instance of the executable running as
//! administrator, reached through a platform byte channel. Protocol: one JSON
//! object per line, strictly request → response.

use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::platform::{Duplex, Platform};
use crate::tools::ToolOutput;

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: u64,
    tool: String,
    args: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<ToolOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireError {
    kind: String,
    message: String,
}

fn error_from_wire(e: WireError) -> Error {
    let m = e.message;
    match e.kind.as_str() {
        "unsupported" => Error::Unsupported(m),
        "not_found" => Error::NotFound(m),
        "invalid_argument" => Error::InvalidArgument(m),
        "elevation_required" => Error::ElevationRequired(m),
        "policy_denied" => Error::PolicyDenied(m),
        "timeout" => Error::Timeout(m),
        "cancelled" => Error::Cancelled(m),
        _ => Error::Platform(m),
    }
}

pub struct WorkerClient {
    channel: BufReader<Box<dyn Duplex>>,
    next_id: u64,
}

impl WorkerClient {
    pub fn new(channel: Box<dyn Duplex>) -> Self {
        Self {
            channel: BufReader::new(channel),
            next_id: 1,
        }
    }

    pub fn call(&mut self, tool: &str, args: &Value) -> Result<ToolOutput> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&Request {
            id,
            tool: tool.to_string(),
            args: args.clone(),
        })
        .map_err(Error::platform)?;
        line.push('\n');
        let io = |e: std::io::Error| Error::Platform(format!("elevated worker channel: {e}"));
        self.channel
            .get_mut()
            .write_all(line.as_bytes())
            .map_err(io)?;
        self.channel.get_mut().flush().map_err(io)?;
        let mut reply = String::new();
        if self.channel.read_line(&mut reply).map_err(io)? == 0 {
            return Err(Error::Platform("elevated worker exited".into()));
        }
        let response: Response = serde_json::from_str(&reply)
            .map_err(|e| Error::Platform(format!("bad worker reply: {e}")))?;
        if response.id != id {
            return Err(Error::Platform(format!(
                "worker replied to request {} instead of {id}",
                response.id
            )));
        }
        match (response.ok, response.error) {
            (Some(out), _) => Ok(out),
            (None, Some(e)) => Err(error_from_wire(e)),
            (None, None) => Err(Error::Platform("empty worker reply".into())),
        }
    }
}

/// Starts the elevated worker if it is not running yet (shows the consent prompt).
pub fn ensure_worker(
    platform: &dyn Platform,
    slot: &Mutex<Option<WorkerClient>>,
    worker_args: &[String],
) -> Result<()> {
    let mut guard = slot
        .lock()
        .map_err(|_| Error::Platform("worker lock poisoned".into()))?;
    if guard.is_none() {
        if !platform.info().can_elevate {
            return Err(Error::Unsupported(
                "this platform cannot start an elevated worker".into(),
            ));
        }
        *guard = Some(WorkerClient::new(
            platform.spawn_elevated_worker(worker_args)?,
        ));
    }
    Ok(())
}

/// Worker side: answer requests until the parent closes the channel.
pub fn serve(dispatcher: &Dispatcher, channel: Box<dyn Duplex>) -> Result<()> {
    let mut channel = BufReader::new(channel);
    let mut line = String::new();
    loop {
        line.clear();
        match channel.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(Error::platform(e)),
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => return Err(Error::Platform(format!("bad request from parent: {e}"))),
        };
        let response = match dispatcher.call(&request.tool, request.args) {
            Ok(out) => Response {
                id: request.id,
                ok: Some(out),
                error: None,
            },
            Err(e) => Response {
                id: request.id,
                ok: None,
                error: Some(WireError {
                    kind: e.kind().into(),
                    message: e.message().into(),
                }),
            },
        };
        let mut text = serde_json::to_string(&response).map_err(Error::platform)?;
        text.push('\n');
        channel
            .get_mut()
            .write_all(text.as_bytes())
            .map_err(Error::platform)?;
        channel.get_mut().flush().map_err(Error::platform)?;
    }
}
