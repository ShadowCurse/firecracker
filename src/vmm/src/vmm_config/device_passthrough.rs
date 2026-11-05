// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::pci::PciSBDF;

/// Errors for device passthrough configuration.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum DevicePassthroughConfigError {
    /// Duplicate device passthrough host device: {0:?}
    DuplicateHost(DevicePassthroughHost),
    /// Invalid device passthrough SBDF: {0}
    InvalidSBDF(String),
}

fn serialize_sbdf_as_str<S: Serializer>(sbdf: &PciSBDF, serializer: S) -> Result<S::Ok, S::Error> {
    // Serialize to string to present in a "0000:01:02.03" format
    serializer.collect_str(&sbdf)
}

fn deserialize_sbdf_from_str<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<PciSBDF, D::Error> {
    let s = String::deserialize(deserializer)?;
    PciSBDF::new_from_str(&s).ok_or_else(|| {
        serde::de::Error::custom(DevicePassthroughConfigError::InvalidSBDF(s.to_string()))
    })
}

/// Host device to pass through
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePassthroughHost {
    /// SBDF of the host PCI device
    Sbdf(
        #[serde(
            serialize_with = "serialize_sbdf_as_str",
            deserialize_with = "deserialize_sbdf_from_str"
        )]
        PciSBDF,
    ),
    /// Sysfs path of the host device (e.g. `/sys/bus/mdev/devices/<uuid>`)
    Path(PathBuf),
}

/// Config for device passthrough
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePassthroughConfig {
    /// ID of the device
    pub id: String,
    /// Host device
    #[serde(flatten)]
    pub host: DevicePassthroughHost,
}

/// Configs for device passthrough
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevicePassthroughConfigs {
    /// configs
    pub configs: Vec<DevicePassthroughConfig>,
}

impl DevicePassthroughConfigs {
    /// Add config to the set. Overwrite existing one if
    /// ids are same.
    pub fn add(
        &mut self,
        config: DevicePassthroughConfig,
    ) -> Result<(), DevicePassthroughConfigError> {
        if self
            .configs
            .iter()
            .any(|b| b.host == config.host && b.id != config.id)
        {
            return Err(DevicePassthroughConfigError::DuplicateHost(config.host));
        }
        if let Some(old_config) = self.configs.iter_mut().find(|b| b.id == config.id) {
            old_config.host = config.host;
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
    fn test_add_device_passthrough_config_and_overwrite() {
        let id1 = DevicePassthroughHost::Sbdf(PciSBDF::new_from_str("01:00.0").unwrap());
        let id2 = DevicePassthroughHost::Path(PathBuf::from("/sys/bus/mdev/devices/uuid"));

        let mut configs = DevicePassthroughConfigs::default();

        configs
            .add(DevicePassthroughConfig {
                id: "dev0".to_string(),
                host: id1.clone(),
            })
            .unwrap();
        assert_eq!(configs.configs.len(), 1);
        assert_eq!(configs.configs[0].host, id1);

        configs
            .add(DevicePassthroughConfig {
                id: "dev0".to_string(),
                host: id2.clone(),
            })
            .unwrap();
        assert_eq!(configs.configs.len(), 1);
        assert_eq!(configs.configs[0].host, id2);

        configs
            .add(DevicePassthroughConfig {
                id: "dev1".to_string(),
                host: id1.clone(),
            })
            .unwrap();
        assert_eq!(configs.configs.len(), 2);
        assert_eq!(configs.configs[0].host, id2);
        assert_eq!(configs.configs[1].host, id1);

        configs
            .add(DevicePassthroughConfig {
                id: "dev1".to_string(),
                host: id2.clone(),
            })
            .unwrap_err();
        assert_eq!(configs.configs.len(), 2);
        assert_eq!(configs.configs[0].host, id2);
        assert_eq!(configs.configs[1].host, id1);
    }
}
