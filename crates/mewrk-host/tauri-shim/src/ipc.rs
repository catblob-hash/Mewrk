//! Channels from the host to whoever asked. In Tauri they cross into a webview as JSON; here they end in a closure.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::Serialize;

#[derive(Clone, Debug)]
pub enum InvokeResponseBody {
    Json(String),
    Raw(Vec<u8>),
}

/// A response that bypasses JSON, for bytes.
pub struct Response {
    body: InvokeResponseBody,
}

impl Response {
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self { body: InvokeResponseBody::Raw(body.into()) }
    }
}

pub trait IpcResponse {
    fn body(self) -> crate::Result<InvokeResponseBody>;
}

impl<T: Serialize> IpcResponse for T {
    fn body(self) -> crate::Result<InvokeResponseBody> {
        Ok(InvokeResponseBody::Json(serde_json::to_string(&self)?))
    }
}

impl IpcResponse for Response {
    fn body(self) -> crate::Result<InvokeResponseBody> {
        Ok(self.body)
    }
}

type Sink = Arc<dyn Fn(InvokeResponseBody) -> crate::Result<()> + Send + Sync>;

pub struct Channel<T = InvokeResponseBody> {
    id: u32,
    sink: Sink,
    message: PhantomData<fn(T)>,
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        Self { id: self.id, sink: self.sink.clone(), message: PhantomData }
    }
}

impl<T> std::fmt::Debug for Channel<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Channel({})", self.id)
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

impl<T> Channel<T> {
    pub fn new(on_message: impl Fn(InvokeResponseBody) -> crate::Result<()> + Send + Sync + 'static) -> Self {
        Self { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), sink: Arc::new(on_message), message: PhantomData }
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl<T: IpcResponse> Channel<T> {
    pub fn send(&self, message: T) -> crate::Result<()> {
        (self.sink)(message.body()?)
    }
}
