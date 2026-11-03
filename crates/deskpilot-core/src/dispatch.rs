//! Routes tool calls: local execution, or the elevated worker when the call
//! asks for `elevated: true`. Audits mutating calls.

use std::sync::Mutex;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::platform::Platform;
use crate::policy::Policy;
use crate::tools::{self, Ctx, ToolOutput, ToolSpec};
use crate::worker::{self, WorkerClient};

pub struct Dispatcher {
    platform: Box<dyn Platform>,
    policy: Policy,
    tools: Vec<ToolSpec>,
    worker: Mutex<Option<WorkerClient>>,
    worker_args: Vec<String>,
}

impl Dispatcher {
    /// `worker_args` are the command-line flags that reproduce `policy` in an elevated worker.
    pub fn new(platform: Box<dyn Platform>, policy: Policy, worker_args: Vec<String>) -> Self {
        Self {
            tools: tools::registry(&platform.info()),
            platform,
            policy,
            worker: Mutex::new(None),
            worker_args,
        }
    }

    pub fn platform(&self) -> &dyn Platform {
        self.platform.as_ref()
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn call(&self, name: &str, mut args: Value) -> Result<ToolOutput> {
        let spec = self
            .tools
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| Error::InvalidArgument(format!("unknown tool '{name}'")))?;
        let elevated = match args.as_object_mut().and_then(|o| o.remove("elevated")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => b,
            Some(other) => {
                return Err(Error::InvalidArgument(format!(
                    "\"elevated\" must be a boolean, got {other}"
                )));
            }
        };

        if elevated && !self.platform.info().elevated && name != "elevate" {
            return self.call_worker(name, &args);
        }
        let ctx = Ctx {
            platform: self.platform.as_ref(),
            policy: &self.policy,
            worker: &self.worker,
            worker_args: &self.worker_args,
        };
        let result = spec.call(&ctx, args.clone());
        if spec.mutating {
            let outcome = match &result {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error:{}", e.kind()),
            };
            self.policy.audit(name, &args, &outcome);
        }
        result
    }

    fn call_worker(&self, name: &str, args: &Value) -> Result<ToolOutput> {
        worker::ensure_worker(self.platform.as_ref(), &self.worker, &self.worker_args)?;
        let mut guard = self
            .worker
            .lock()
            .map_err(|_| Error::Platform("worker lock poisoned".into()))?;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::Platform("elevated worker is not running".into()))?;
        let result = client.call(name, args);
        if matches!(&result, Err(Error::Platform(m)) if m.starts_with("elevated worker")) {
            // The channel broke (worker closed or crashed): forget it so the next call restarts it.
            *guard = None;
        }
        result
    }
}
