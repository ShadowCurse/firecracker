use serde::{Deserialize, Serialize};

/// Config for VFIO device
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct VfioConfig {
    /// Id of the device
    pub id: String,
    /// Path to the device
    pub path: String,
}

/// Config for VFIO passthrough devices
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct VfioConfigs {
    /// VFIO configs
    pub configs: Vec<VfioConfig>,
}

impl VfioConfigs {
    /// Create new empty type
    pub fn new() -> Self {
        Self {
            configs: Default::default(),
        }
    }

    /// Add config to the set. Overwrite existing one if
    /// ids are same.
    pub fn add(&mut self, config: VfioConfig) {
        if let Some(old_config) = self.configs.iter_mut().find(|b| b.id == config.id) {
            old_config.path = config.path;
        } else {
            self.configs.push(config);
        }
    }
}
