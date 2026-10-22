//! Bridges an interactive LXD exec session to the UI.
//!
//! A background thread owns both websockets of the exec operation: the
//! pseudo-terminal stream (fd "0") and the control channel used for window
//! resizes. The UI talks to it through channels, so this module knows nothing
//! about how the terminal is drawn.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use serde_json::json;
use tungstenite::{Message, WebSocket};

use crate::Client;

/// How long a read on the terminal stream waits before checking for input.
const POLL_INTERVAL: Duration = Duration::from_millis(15);

#[derive(Debug)]
pub enum ConsoleInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, PartialEq)]
pub enum ConsoleEvent {
    Output(Vec<u8>),
    /// The shell exited or LXD closed the session.
    Closed,
    Error(String),
}

/// Handle for sending keystrokes and resizes to a running console. Dropping
/// it ends the session.
pub struct Console {
    input: Sender<ConsoleInput>,
}

impl Console {
    /// Starts `command` in `instance` and returns the input handle and the
    /// event stream. Connection errors arrive as `ConsoleEvent::Error`.
    pub fn spawn(
        client: Client,
        instance: String,
        command: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> (Self, async_channel::Receiver<ConsoleEvent>) {
        let (input, input_rx) = mpsc::channel();
        let (events, events_rx) = async_channel::unbounded();

        thread::spawn(move || {
            let command: Vec<&str> = command.iter().map(String::as_str).collect();
            let result = run(&client, &instance, &command, cols, rows, &input_rx, &events);
            let event = match result {
                Ok(()) => ConsoleEvent::Closed,
                Err(e) => ConsoleEvent::Error(e),
            };
            let _ = events.send_blocking(event);
        });

        (Self { input }, events_rx)
    }

    pub fn write(&self, data: &[u8]) {
        let _ = self.input.send(ConsoleInput::Data(data.to_vec()));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.input.send(ConsoleInput::Resize { cols, rows });
    }
}

fn run(
    client: &Client,
    instance: &str,
    command: &[&str],
    cols: u16,
    rows: u16,
    input: &Receiver<ConsoleInput>,
    events: &async_channel::Sender<ConsoleEvent>,
) -> Result<(), String> {
    let session = client.exec_interactive(instance, command, cols, rows)?;
    // LXD starts the process once every websocket of the operation is
    // connected; connect the control channel first like the lxc client does.
    let mut control = client.websocket(&session.operation_id, &session.control_secret)?;
    let mut data = client.websocket(&session.operation_id, &session.data_secret)?;
    data.get_ref()
        .set_read_timeout(Some(POLL_INTERVAL))
        .map_err(|e| e.to_string())?;

    loop {
        match data.read() {
            Ok(Message::Binary(bytes)) => send(events, bytes.to_vec())?,
            Ok(Message::Text(text)) => send(events, text.as_bytes().to_vec())?,
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break;
            }
            Err(e) => return Err(format!("console stream failed: {e}")),
        }

        match pump_input(input, &mut data, &mut control) {
            Ok(true) => {}
            // The UI dropped its handle: hang up.
            Ok(false) => {
                let _ = data.close(None);
                let _ = control.close(None);
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Forwards queued input. Returns false once the UI side has gone away.
fn pump_input(
    input: &Receiver<ConsoleInput>,
    data: &mut WebSocket<UnixStream>,
    control: &mut WebSocket<UnixStream>,
) -> Result<bool, String> {
    loop {
        match input.try_recv() {
            Ok(ConsoleInput::Data(bytes)) => data
                .send(Message::binary(bytes))
                .map_err(|e| format!("console write failed: {e}"))?,
            Ok(ConsoleInput::Resize { cols, rows }) => {
                let message = json!({
                    "command": "window-resize",
                    "args": { "width": cols.to_string(), "height": rows.to_string() },
                });
                control
                    .send(Message::text(message.to_string()))
                    .map_err(|e| format!("console resize failed: {e}"))?;
            }
            Err(TryRecvError::Empty) => return Ok(true),
            Err(TryRecvError::Disconnected) => return Ok(false),
        }
    }
}

fn send(events: &async_channel::Sender<ConsoleEvent>, bytes: Vec<u8>) -> Result<(), String> {
    events
        .send_blocking(ConsoleEvent::Output(bytes))
        .map_err(|_| "console window closed".to_owned())
}
