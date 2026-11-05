//! The test suite's mock model provider on a port of your choosing, for driving a real window by hand: `mock_provider 18765`.
//! Point a provider at `http://127.0.0.1:18765/v1` with the `openai_chat` family; a loopback address needs no key.

#[path = "../tests/suite/mock.rs"]
#[allow(dead_code)]
mod mock;

fn main() {
    let port = std::env::args().nth(1).and_then(|port| port.parse().ok()).unwrap_or(18765);
    let served = mock::serve(port);
    println!("{}", served.address);
    loop {
        std::thread::park();
    }
}
