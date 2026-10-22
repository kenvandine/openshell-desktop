//! Interactive commands in a sandbox through the gateway.
//!
//! Unlike an LXD exec, these run under the sandbox supervisor: the sandbox
//! policy applies, network traffic goes through OpenShell's proxy, and
//! provider credentials are injected. This is how agents and shells should
//! be run.

use openshell_core::proto::{
    ExecSandboxInput, ExecSandboxRequest, ExecSandboxWindowResize, exec_sandbox_event,
    exec_sandbox_input,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{Error, Gateway, workspace};

/// What to run and how big the terminal starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOptions {
    pub command: Vec<String>,
    /// Blank uses the sandbox's working directory.
    pub workdir: String,
    pub environment: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// Keystrokes and resizes for a running command. Dropping every clone ends
/// the command's stdin.
#[derive(Debug, Clone)]
pub struct ExecInput(mpsc::UnboundedSender<ExecSandboxInput>);

impl ExecInput {
    /// A handle whose input arrives on the returned receiver, for relaying
    /// input typed before a session is up; see `forward`.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<ExecSandboxInput>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self(sender), receiver)
    }

    /// Passes on a message taken from another handle's receiver.
    pub fn forward(&self, message: ExecSandboxInput) {
        let _ = self.0.send(message);
    }

    pub fn write(&self, data: &[u8]) {
        let _ = self.0.send(ExecSandboxInput {
            payload: Some(exec_sandbox_input::Payload::Stdin(data.to_vec())),
        });
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.0.send(ExecSandboxInput {
            payload: Some(exec_sandbox_input::Payload::Resize(
                ExecSandboxWindowResize {
                    cols: cols.into(),
                    rows: rows.into(),
                },
            )),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutput {
    /// Terminal output; with a TTY stdout and stderr are one stream.
    Data(Vec<u8>),
    Exit(i32),
}

/// The output side of an interactive command.
pub struct ExecOutputStream(tonic::Streaming<openshell_core::proto::ExecSandboxEvent>);

impl ExecOutputStream {
    /// The next output, or `None` once the gateway closes the stream.
    pub async fn next(&mut self) -> Result<Option<ExecOutput>, Error> {
        loop {
            let Some(event) = self.0.message().await? else {
                return Ok(None);
            };
            match event.payload {
                Some(exec_sandbox_event::Payload::Stdout(out)) => {
                    return Ok(Some(ExecOutput::Data(out.data)));
                }
                Some(exec_sandbox_event::Payload::Stderr(err)) => {
                    return Ok(Some(ExecOutput::Data(err.data)));
                }
                Some(exec_sandbox_event::Payload::Exit(exit)) => {
                    return Ok(Some(ExecOutput::Exit(exit.exit_code)));
                }
                None => continue,
            }
        }
    }
}

impl Gateway {
    /// Runs `command` in `sandbox` without a terminal and returns its exit
    /// status; for quick checks such as whether a binary is installed.
    pub async fn exec_status(&mut self, sandbox: &str, command: Vec<String>) -> Result<i32, Error> {
        let mut request = start_request(
            sandbox,
            ExecOptions {
                command,
                ..Default::default()
            },
        );
        request.tty = false;
        request.no_login_shell = true;
        let mut events = self.client.exec_sandbox(request).await?.into_inner();
        while let Some(event) = events.message().await? {
            if let Some(exec_sandbox_event::Payload::Exit(exit)) = event.payload {
                return Ok(exit.exit_code);
            }
        }
        Err(Error::Rpc(tonic::Status::unavailable(
            "the command ended without an exit status",
        )))
    }

    /// Starts `options.command` with a pseudo-terminal in `sandbox`.
    /// Dropping the output stream cancels the call, which ends the command.
    pub async fn exec_interactive(
        &mut self,
        sandbox: &str,
        options: ExecOptions,
    ) -> Result<(ExecInput, ExecOutputStream), Error> {
        let (input, input_rx) = mpsc::unbounded_channel();
        input
            .send(ExecSandboxInput {
                payload: Some(exec_sandbox_input::Payload::Start(start_request(
                    sandbox, options,
                ))),
            })
            .expect("receiver is held above");
        let response = self
            .client
            .exec_sandbox_interactive(UnboundedReceiverStream::new(input_rx))
            .await?
            .into_inner();
        Ok((ExecInput(input), ExecOutputStream(response)))
    }
}

fn start_request(sandbox: &str, options: ExecOptions) -> ExecSandboxRequest {
    ExecSandboxRequest {
        workspace_scope: workspace(),
        sandbox: sandbox.into(),
        command: options.command,
        workdir: options.workdir,
        environment: options.environment.into_iter().collect(),
        tty: true,
        cols: options.cols.into(),
        rows: options.rows.into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_request_asks_for_a_sized_tty() {
        let request = start_request(
            "dev",
            ExecOptions {
                command: vec!["claude".into()],
                workdir: "/sandbox/project".into(),
                environment: vec![("TERM".into(), "xterm-256color".into())],
                cols: 120,
                rows: 40,
            },
        );
        assert_eq!(request.sandbox, "dev");
        assert_eq!(request.command, ["claude"]);
        assert_eq!(request.workdir, "/sandbox/project");
        assert_eq!(request.environment["TERM"], "xterm-256color");
        assert!(request.tty);
        assert_eq!((request.cols, request.rows), (120, 40));
        assert!(request.workspace_scope.is_some());
    }
}
