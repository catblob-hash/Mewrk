//! `lib/modelCapabilities.ts` as the client publishes it, with what the pages say often.

pub use mewrk_client::model_capabilities::*;

use mewrk_client::model::{ProviderFamily, ReasoningContent};
use serde_json::Value;

/// A new model's reasoning form: the protocol's own default, because the stored value is always concrete.
pub fn default_reasoning_content(family: ProviderFamily) -> ReasoningContent {
    normalize_reasoning_content(&Value::Null, family)
}
