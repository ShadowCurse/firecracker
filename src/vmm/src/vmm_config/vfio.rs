use std::path::Path;

use serde::{Deserialize, Serialize};

/// Errors for VFIO device configuration.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioConfigError {
    /// Cannot verify path to the VFIO device
    PathDoesNotExist,
}

/// Config for VFIO device
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct VfioConfig {
    /// ID of the device
    pub id: String,
    /// Sysfs path to the PCI device
    pub path_on_host: String,
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
    pub fn add(&mut self, config: VfioConfig) -> Result<(), VfioConfigError> {
        // A simple sanity check. This does not guarantee that the device will be successfully
        // initialized later on.
        if !Path::new(&config.path_on_host).exists() {
            return Err(VfioConfigError::PathDoesNotExist);
        }
        if let Some(old_config) = self.configs.iter_mut().find(|b| b.id == config.id) {
            old_config.path_on_host = config.path_on_host;
        } else {
            self.configs.push(config);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_vfio_config() {
        let mut configs = VfioConfigs::new();
        let cfg = VfioConfig {
            id: "dev0".to_string(),
            path_on_host: "/sys/bus/pci/devices/0000:00:1f.0".to_string(),
        };
        configs.add(cfg.clone()).unwrap();
        assert_eq!(configs.configs.len(), 1);
        assert_eq!(configs.configs[0], cfg);
    }

    #[test]
    fn test_add_vfio_config_overwrite() {
        let mut configs = VfioConfigs::new();
        configs
            .add(VfioConfig {
                id: "dev0".to_string(),
                path_on_host: "/old/path".to_string(),
            })
            .unwrap();
        configs
            .add(VfioConfig {
                id: "dev0".to_string(),
                path_on_host: "/new/path".to_string(),
            })
            .unwrap();
        assert_eq!(configs.configs.len(), 1);
        assert_eq!(configs.configs[0].path_on_host, "/new/path");
    }

    #[test]
    fn test_add_vfio_config_empty_path() {
        let mut configs = VfioConfigs::new();
        let err = configs
            .add(VfioConfig {
                id: "dev0".to_string(),
                path_on_host: String::new(),
            })
            .unwrap_err();
        assert!(matches!(err, VfioConfigError::PathDoesNotExist));
    }
}
