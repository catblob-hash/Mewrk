//! A model provider on a socket, speaking OpenAI's chat completions: the far end of a whole run, for a test or a real window.
//! What it answers depends on the last message: a word in square brackets asks for a behaviour, and anything else gets
//! a reply with one of everything the timeline has to draw.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub const RICH_REPLY: &str = "# Result\n\nHere is **bold**, `code`, and a [link](https://example.com).\n\n- first\n- second\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nAn inline $x^2 + y^2 = z^2$ and a display:\n\n$$\\frac{a}{b} = \\sum_{i=0}^{n} i$$\n";

pub struct Mock {
    pub address: String,
    /// Every request body, in the order they came.
    pub requests: Arc<Mutex<Vec<Value>>>,
    /// While true, replies that were asked to wait with `[slow]` wait.
    pub hold: Arc<AtomicBool>,
}

fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn delta(value: Value) -> String {
    json!({ "choices": [{ "delta": value }] }).to_string()
}

fn tool_call(name: &str, arguments: Value) -> Vec<String> {
    let id = format!("call_{name}");
    vec![
        delta(json!({ "tool_calls": [{ "index": 0, "id": id, "type": "function", "function": { "name": name, "arguments": arguments.to_string() } }] })),
        json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }).to_string(),
    ]
}

/// The frames of the reply to one request, and whether `[slow]` asked it to wait.
pub fn reply(body: &Value) -> (Vec<String>, bool) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let said = text_of(&last);
    let finish = || json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }).to_string();
    let says = |word: &str| said.contains(word);
    let mut slow = false;
    let mut frames = if last["role"] == "tool" {
        vec![delta(json!({ "content": format!("The tool said: {}", text_of(&last).chars().take(60).collect::<String>()) })), finish()]
    } else if says("[read]") {
        tool_call("read", json!({ "path": "README.md" }))
    } else if says("[write]") {
        tool_call("write", json!({ "path": "written.txt", "content": "from the model\n" }))
    } else if says("[shell]") {
        tool_call("bash", json!({ "command": "echo from-the-shell", "description": "say something" }))
    } else if says("[ask]") {
        tool_call("ask_user", json!({ "questions": [{ "question": "Which one?", "header": "Pick", "multiSelect": false, "options": [{ "label": "Left", "description": "the left one" }, { "label": "Right", "description": "the right one" }] }] }))
    } else if says("[fail]") {
        return (vec!["!400".to_owned()], false);
    } else {
        slow = says("[slow]");
        let mut frames = vec![delta(json!({ "reasoning_content": "Let me think about it." }))];
        // In pieces, the way a stream arrives.
        let chars: Vec<char> = RICH_REPLY.chars().collect();
        frames.extend(chars.chunks(24).map(|piece| delta(json!({ "content": piece.iter().collect::<String>() }))));
        frames.push(finish());
        frames
    };
    frames.push(r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5}}"#.to_owned());
    frames.push("[DONE]".to_owned());
    (frames, slow)
}

fn answer(mut socket: TcpStream, requests: Arc<Mutex<Vec<Value>>>, hold: Arc<AtomicBool>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let header_end = loop {
            if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            match socket.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
        let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length:").and_then(|value| value.trim().parse().ok())).unwrap_or(0);
        while buffer.len() < header_end + length {
            match socket.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let body: Value = serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap_or(Value::Null);
        buffer.drain(..header_end + length);
        requests.lock().unwrap().push(body.clone());
        let (frames, slow) = reply(&body);
        if frames.first().is_some_and(|frame| frame == "!400") {
            let message = br#"{"error":{"message":"the mock was told to fail","type":"invalid_request_error"}}"#;
            let _ = socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", message.len()).as_bytes());
            let _ = socket.write_all(message);
            let _ = socket.flush();
            continue;
        }
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
        let _ = socket.flush();
        for (index, frame) in frames.iter().enumerate() {
            // The reasoning goes out, then a slow reply waits with the run visibly live.
            while slow && index == 1 && hold.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let frame = format!("data: {frame}\n\n");
            if socket.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).is_err() {
                return;
            }
            let _ = socket.flush();
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = socket.write_all(b"0\r\n\r\n");
    }
}

pub fn serve(port: u16) -> Mock {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let hold = Arc::new(AtomicBool::new(false));
    let (seen, held) = (requests.clone(), hold.clone());
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(socket) = socket else { return };
            let (seen, held) = (seen.clone(), held.clone());
            std::thread::spawn(move || answer(socket, seen, held));
        }
    });
    Mock { address, requests, hold }
}
