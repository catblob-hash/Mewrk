//! The in-process sidecar on stdio: frames in on stdin, frames out on stdout, as the Node sidecar was.
//!
//! This exists so upstream's own conformance run can drive this crate as it drove the original:
//! `MEWRK_AISDK_BIN=target/debug/examples/stdio node aisdk-service/selfcheck.mjs`.

use std::io::{BufRead, Write};
use std::sync::Mutex;

fn main() {
    let stdout = Mutex::new(std::io::stdout());
    let sidecar = mewrk_aisdk::Sidecar::start(
        Box::new(move |line| {
            let mut out = stdout.lock().unwrap_or_else(|p| p.into_inner());
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }),
        Box::new(|message| {
            eprintln!("[aisdk] {message}");
            std::process::exit(2);
        }),
    )
    .expect("the sidecar starts");
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let shutdown = line.contains("\"type\":\"shutdown\"");
        sidecar.send_line(&line);
        if shutdown {
            break;
        }
    }
    sidecar.stop();
}
