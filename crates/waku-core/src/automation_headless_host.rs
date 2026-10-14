//! The backend-only executor is a thread in the existing daemon. It uses the
//! same authenticated client/executor contract as the native application.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use waku_client::{AutomationExecutorMessage, DaemonClient};
use waku_protocol::{
    AutomationCall, AutomationCommand, AutomationExecutorEvent, AutomationExecutorOutcome,
    AutomationExecutorReply, AutomationResult, AutomationStream,
};

pub fn start(
    backend: Arc<crate::daemon::WakuBackend>,
    address: String,
    token: String,
    build: String,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    anyhow::ensure!(
        cfg!(debug_assertions),
        "headless automation requires a debug build"
    );
    std::thread::Builder::new()
        .name("waku-headless-executor".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let client = DaemonClient::connect(&address, token)?;
                let requests = client.subscribe_automation_executor();
                backend.configure_automation_headless(build);
                let instance = backend.automation_headless_identity();
                let instance_id = instance.instance_id;
                match client.automation(AutomationCommand::RegisterExecutor { instance })? {
                    AutomationResult::Registered { .. } => {}
                    _ => anyhow::bail!("headless executor registration failed"),
                }
                let mut model_interested = false;
                while !shutdown.load(Ordering::Acquire) {
                    match requests.recv_timeout(Duration::from_millis(100)) {
                        Ok(AutomationExecutorMessage::Request(request)) => {
                            let mutation = matches!(request.call, AutomationCall::Invoke { .. });
                            let outcome = match backend.automation_headless_call(request.call) {
                                Ok(execution) => {
                                    if mutation && model_interested {
                                        client.publish_automation_event(
                                            AutomationExecutorEvent {
                                                instance_id,
                                                stream: AutomationStream::Model,
                                                observation: execution.observation.clone(),
                                            },
                                        )?;
                                    }
                                    AutomationExecutorOutcome::Ok { execution }
                                }
                                Err(error) => AutomationExecutorOutcome::Error { error },
                            };
                            client.reply_automation_executor(AutomationExecutorReply {
                                request_id: request.request_id,
                                outcome,
                            })?;
                        }
                        Ok(AutomationExecutorMessage::Interest { streams, .. }) => {
                            model_interested = streams.contains(&AutomationStream::Model);
                            if model_interested {
                                if let Ok(execution) =
                                    backend.automation_headless_call(AutomationCall::Snapshot {
                                        sections: vec![],
                                    })
                                {
                                    client.publish_automation_event(AutomationExecutorEvent {
                                        instance_id,
                                        stream: AutomationStream::Model,
                                        observation: execution.observation,
                                    })?;
                                }
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                client.disconnect();
                Ok(())
            })();
            if result.is_err() {
                // Never print transport configuration or the bearer capability.
                eprintln!("waku-daemon: headless executor stopped unexpectedly");
                shutdown.store(true, Ordering::Release);
            }
        })
        .map_err(Into::into)
}
