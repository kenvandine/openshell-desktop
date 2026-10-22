//! Drives the client against a fake LXD daemon on a temporary Unix socket.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use openshell_lxd::Client;
use openshell_lxd::console::{Console, ConsoleEvent};
use serde_json::{Value, json};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

/// What the fake daemon saw, for assertions.
#[derive(Debug)]
enum Seen {
    Exec(Value),
    Control(Value),
}

fn read_request(stream: &mut UnixStream) -> (String, Vec<String>, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap().to_owned();
    let headers: Vec<String> = lines.map(str::to_owned).collect();
    let length = headers
        .iter()
        .find_map(|h| h.strip_prefix("Content-Length: "))
        .map_or(0, |v| v.parse().unwrap());
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (request_line, headers, body)
}

fn respond(mut stream: UnixStream, body: Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn upgrade(mut stream: UnixStream, headers: &[String]) -> WebSocket<UnixStream> {
    let key = headers
        .iter()
        .find_map(|h| {
            h.strip_prefix("Sec-WebSocket-Key: ")
                .or(h.strip_prefix("sec-websocket-key: "))
        })
        .unwrap();
    let accept = tungstenite::handshake::derive_accept_key(key.trim().as_bytes());
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .unwrap();
    WebSocket::from_raw_socket(stream, Role::Server, None)
}

fn serve(stream: UnixStream, seen: Sender<Seen>) {
    let mut stream = stream;
    let (request_line, headers, body) = read_request(&mut stream);
    let path = request_line.split_whitespace().nth(1).unwrap().to_owned();

    match path.as_str() {
        "/1.0" => respond(
            stream,
            json!({"type": "sync", "metadata": {"environment": {"server_version": "6.5"}}}),
        ),
        "/1.0/instances?recursion=2" => respond(
            stream,
            json!({"type": "sync", "metadata": [{
                "name": "c1", "status": "Running", "type": "container",
                "profiles": ["default"],
                "expanded_config": {"limits.memory": "2GiB"},
                "expanded_devices": {"work": {"type": "disk", "source": "/home/me/work", "path": "/workspace"}},
                "state": {"memory": {"usage": 104857600}}
            }]}),
        ),
        "/1.0/instances/missing/exec" => respond(
            stream,
            json!({"type": "error", "error": "Instance not found", "error_code": 404}),
        ),
        "/1.0/instances/c1/exec" => {
            seen.send(Seen::Exec(serde_json::from_slice(&body).unwrap()))
                .unwrap();
            respond(
                stream,
                json!({"type": "async", "operation": "/1.0/operations/op1",
                       "metadata": {"id": "op1", "metadata": {"fds": {"0": "s0", "control": "sc"}}}}),
            );
        }
        "/1.0/operations/op1/websocket?secret=sc" => {
            let mut control = upgrade(stream, &headers);
            while let Ok(Message::Text(text)) = control.read() {
                seen.send(Seen::Control(serde_json::from_str(&text).unwrap()))
                    .unwrap();
            }
        }
        "/1.0/operations/op1/websocket?secret=s0" => {
            // An echo "shell": prefixes input, and exits on "exit".
            let mut data = upgrade(stream, &headers);
            while let Ok(message) = data.read() {
                let Message::Binary(bytes) = message else {
                    continue;
                };
                if &bytes[..] == b"exit" {
                    let _ = data.close(None);
                    let _ = data.flush();
                    break;
                }
                let mut reply = b"echo:".to_vec();
                reply.extend_from_slice(&bytes);
                data.send(Message::binary(reply)).unwrap();
            }
        }
        other => panic!("unexpected request {other}"),
    }
}

fn fake_lxd() -> (tempfile::TempDir, Client, mpsc::Receiver<Seen>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unix.socket");
    let listener = UnixListener::bind(&path).unwrap();
    let (seen_tx, seen_rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let seen = seen_tx.clone();
            thread::spawn(move || serve(stream.unwrap(), seen));
        }
    });
    (dir, Client::new(&path), seen_rx)
}

fn next_event(events: &async_channel::Receiver<ConsoleEvent>) -> ConsoleEvent {
    let (tx, rx) = mpsc::channel();
    let events = events.clone();
    thread::spawn(move || {
        let _ = tx.send(events.recv_blocking());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a console event")
        .expect("console event channel closed")
}

#[test]
fn reports_status_and_instances() {
    let (_dir, client, _) = fake_lxd();
    match client.status() {
        openshell_lxd::LxdStatus::Active { server_version } => {
            assert_eq!(server_version.as_deref(), Some("6.5"))
        }
        other => panic!("expected active, got {other:?}"),
    }

    let instances = client.instances().unwrap();
    assert_eq!(instances.len(), 1);
    let c1 = &instances[0];
    assert_eq!(
        (c1.name.as_str(), c1.status.as_str(), c1.kind.as_str()),
        ("c1", "Running", "container")
    );
    assert_eq!(c1.expanded_config["limits.memory"], "2GiB");
    assert_eq!(c1.expanded_devices["work"]["path"], "/workspace");
    assert_eq!(
        c1.state.as_ref().unwrap().memory.as_ref().unwrap().usage,
        104857600
    );
}

#[test]
fn console_round_trip() {
    let (_dir, client, seen) = fake_lxd();
    let (console, events) = Console::spawn(client, "c1".into(), vec!["/bin/bash".into()], 100, 30);

    let Seen::Exec(exec) = seen.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected the exec request first");
    };
    assert_eq!(exec["command"], json!(["/bin/bash"]));
    assert_eq!(exec["interactive"], json!(true));
    assert_eq!(exec["wait-for-websocket"], json!(true));
    assert_eq!(
        (exec["width"].clone(), exec["height"].clone()),
        (json!(100), json!(30))
    );

    console.write(b"ls\r");
    assert_eq!(
        next_event(&events),
        ConsoleEvent::Output(b"echo:ls\r".to_vec())
    );

    console.resize(120, 40);
    let Seen::Control(control) = seen.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected a control message");
    };
    assert_eq!(
        control,
        json!({"command": "window-resize", "args": {"width": "120", "height": "40"}})
    );

    console.write(b"exit");
    assert_eq!(next_event(&events), ConsoleEvent::Closed);
}

#[test]
fn console_reports_lxd_errors() {
    let (_dir, client, _) = fake_lxd();
    let (_console, events) =
        Console::spawn(client, "missing".into(), vec!["/bin/bash".into()], 80, 24);
    assert_eq!(
        next_event(&events),
        ConsoleEvent::Error("Instance not found".into())
    );
}
