//! Tauri's shared Tokio runtime.

use std::future::Future;
use std::sync::OnceLock;

pub use tokio::task::JoinHandle;
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("mewrk-host")
            .build()
            .expect("the host's async runtime could not start")
    })
}

pub fn handle() -> tokio::runtime::Handle {
    runtime().handle().clone()
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future)
}

pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    runtime().spawn_blocking(f)
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}
