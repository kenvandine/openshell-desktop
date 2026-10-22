//! Terminal sessions for the console window.
//!
//! A session is either a command run through the OpenShell gateway, under the
//! sandbox policy with provider credentials injected, or the LXD debug
//! console, which bypasses both. The console window only sees `Session` and
//! `SessionEvent`, so it draws either the same way.

use openshell_gateway_client::{Config, ExecInput, ExecOptions, ExecOutput, Gateway};
use openshell_lxd::console::{Console, ConsoleEvent};

/// libghostty-vt speaks xterm; images rarely ship Ghostty's own terminfo.
const TERM: &str = "xterm-256color";

#[derive(Debug, PartialEq)]
pub enum SessionEvent {
    Output(Vec<u8>),
    /// The command exited, with its code when the gateway reports one.
    Ended(Option<i32>),
    Error(String),
}

/// Input side of a running session. Dropping it ends the session.
pub enum Session {
    Gateway(ExecInput),
    Lxd(Console),
}

impl Session {
    pub fn write(&self, data: &[u8]) {
        match self {
            Session::Gateway(input) => input.write(data),
            Session::Lxd(console) => console.write(data),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        match self {
            Session::Gateway(input) => input.resize(cols, rows),
            Session::Lxd(console) => console.resize(cols, rows),
        }
    }

    /// Runs `options.command` in `sandbox` through the gateway. A thread with its own
    /// runtime owns the call, so a slow session never holds up the sandbox
    /// list; it ends when the command exits or this session is dropped.
    pub fn gateway(
        sandbox: String,
        mut options: ExecOptions,
        cols: u16,
        rows: u16,
    ) -> (Self, async_channel::Receiver<SessionEvent>) {
        options.cols = cols;
        options.rows = rows;
        options.environment.push(("TERM".into(), TERM.into()));
        let (events, events_rx) = async_channel::unbounded();
        // The UI writes before the call is up; buffer that input here and
        // hand it over once the gateway's input handle exists.
        let (input, mut input_rx) = ExecInput::channel();
        std::thread::Builder::new()
            .name("gateway-shell".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let result = runtime.block_on(run(&sandbox, options, &mut input_rx, &events));
                let event = match result {
                    Ok(code) => SessionEvent::Ended(code),
                    Err(error) => SessionEvent::Error(error),
                };
                let _ = events.send_blocking(event);
            })
            .expect("spawn gateway shell thread");
        (Session::Gateway(input), events_rx)
    }

    /// A shell in the LXD instance itself, outside the sandbox policy.
    pub fn lxd(
        instance: String,
        command: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> (Self, async_channel::Receiver<SessionEvent>) {
        let (console, console_events) = Console::spawn(
            openshell_lxd::Client::default(),
            instance,
            command,
            cols,
            rows,
        );
        let (events, events_rx) = async_channel::unbounded();
        gtk::glib::spawn_future_local(async move {
            while let Ok(event) = console_events.recv().await {
                let event = match event {
                    ConsoleEvent::Output(bytes) => SessionEvent::Output(bytes),
                    ConsoleEvent::Closed => SessionEvent::Ended(None),
                    ConsoleEvent::Error(error) => SessionEvent::Error(error),
                };
                if events.send(event).await.is_err() {
                    break;
                }
            }
        });
        (Session::Lxd(console), events_rx)
    }
}

type Input = openshell_gateway_client::proto::ExecSandboxInput;

async fn run(
    sandbox: &str,
    options: ExecOptions,
    input: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
    events: &async_channel::Sender<SessionEvent>,
) -> Result<Option<i32>, String> {
    let mut gateway = Gateway::connect(&Config::discover())
        .await
        .map_err(|e| e.to_string())?;
    let (remote, mut output) = gateway
        .exec_interactive(sandbox, options)
        .await
        .map_err(|e| e.to_string())?;
    let mut exit = None;
    loop {
        tokio::select! {
            received = input.recv() => match received {
                Some(message) => remote.forward(message),
                // The window closed: dropping the output stream cancels the
                // call, which ends the command.
                None => return Ok(exit),
            },
            next = output.next() => match next.map_err(|e| e.to_string())? {
                Some(ExecOutput::Data(bytes)) => {
                    if events.send(SessionEvent::Output(bytes)).await.is_err() {
                        return Ok(exit);
                    }
                }
                Some(ExecOutput::Exit(code)) => exit = Some(code),
                None => return Ok(exit),
            },
        }
    }
}
