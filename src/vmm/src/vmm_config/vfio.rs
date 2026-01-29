use serde::{Deserialize, Serialize};

/// Config for VFIO passthrough devices
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct VfioConfig {
    /// Paths to VFIO devices
    pub paths: Vec<String>,
}
