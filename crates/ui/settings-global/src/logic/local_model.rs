//! `lib/localModel.ts`: what the renderer knows of the local helper model. The host keeps downloading whether or not the
//! page is open, so this lives for the app's lifetime and takes the host's `localModelChanged` pushes.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum VariantId {
    Ane,
    Mlx,
    Llama,
}

impl VariantId {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Ane => "ane",
            Self::Mlx => "mlx",
            Self::Llama => "llama",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Unavailable {
    NeedsAppleSilicon,
    NoNeuralEngine,
    NeedsMacos14,
    NeedsMacos15,
    NotInThisBuild,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DownloadSource {
    HuggingFace,
    HfMirror,
    Mirror,
    Release,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PrepareStep {
    Compile,
    Verify,
    Unpack,
    /// A step this build has no words of its own for.
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum Phase {
    Missing,
    Unsupported { reason: Unavailable },
    Downloading { received: u64, total: u64, source: DownloadSource },
    Preparing { step: PrepareStep, done: u64, total: u64 },
    Ready,
    Failed { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VariantStatus {
    pub id: VariantId,
    #[serde(flatten)]
    pub phase: Phase,
    pub download_bytes: u64,
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub chip: Option<String>,
    pub model: Option<String>,
    pub os_version: Option<String>,
    #[serde(default)]
    pub apple_silicon: bool,
    pub neural_engine_cores: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelStatus {
    pub machine: Machine,
    pub variants: Vec<VariantStatus>,
    pub active: Option<VariantId>,
    pub recommended: Option<VariantId>,
    #[serde(default)]
    pub warming: bool,
    #[serde(default)]
    pub loading: bool,
    pub device: Option<String>,
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub running: u32,
    #[serde(default)]
    pub queued: u32,
    #[serde(default)]
    pub slots: u32,
    #[serde(default)]
    pub context: u32,
    #[serde(default)]
    pub disk_bytes: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Task {
    Title,
    Shell,
    Error,
}

impl Task {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Shell => "shell",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DefaultPrompts {
    pub title: String,
    pub shell: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptReport {
    pub tokens: u64,
    pub cache_bytes: Option<u64>,
    pub max_tokens: u64,
}

pub fn percent_of(done: u64, total: u64) -> u64 {
    if total > 0 { (((done as f64 / total as f64) * 100.0).round() as u64).min(100) } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // localModel.test.ts: the status shape the host pushes.
    #[test]
    fn a_status_reads_each_phase_from_the_wire() {
        let status: LocalModelStatus = serde_json::from_value(json!({
            "machine": {"chip": "Apple M4", "model": null, "osVersion": "15.4", "appleSilicon": true, "neuralEngineCores": 16},
            "variants": [
                {"id": "ane", "phase": "downloading", "received": 5, "total": 10, "source": "hfMirror", "downloadBytes": 10, "diskBytes": 0},
                {"id": "mlx", "phase": "ready", "downloadBytes": 10, "diskBytes": 9},
                {"id": "llama", "phase": "unsupported", "reason": "notInThisBuild", "downloadBytes": 10, "diskBytes": 0}
            ],
            "active": "mlx", "recommended": "ane", "warming": false, "loading": false, "device": "gpu", "loaded": true,
            "running": 0, "queued": 0, "slots": 2, "context": 1024, "diskBytes": 9, "lastError": null
        })).unwrap();
        assert_eq!(status.variants[0].phase, Phase::Downloading { received: 5, total: 10, source: DownloadSource::HfMirror });
        assert_eq!(status.variants[1].phase, Phase::Ready);
        assert_eq!(status.variants[2].phase, Phase::Unsupported { reason: Unavailable::NotInThisBuild });
        assert_eq!(status.active, Some(VariantId::Mlx));
        assert_eq!(percent_of(5, 10), 50);
        assert_eq!(percent_of(5, 0), 0);
    }
}
