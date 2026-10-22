//! Resource admission acknowledgement.
//!
//! The gateway activates an external driver only when the driver echoes back
//! exactly the admission policy the operator configured for it, encoded as
//! `v1:<json>`. The JSON shape mirrors OpenShell's `DriverAdmissionConfig`
//! (field order matters, since the gateway compares the parsed policy).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceAdmissionConfig {
    pub enabled: bool,
    pub required_labels: BTreeMap<String, String>,
}

impl Default for ResourceAdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            required_labels: BTreeMap::from([
                ("openshell.ai/sandbox-attachable".into(), "true".into()),
                (
                    "openshell.ai/sandbox-attachable-workspace".into(),
                    "${workspace}".into(),
                ),
            ]),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionConfig {
    pub allow_driver_config: bool,
    pub resource_admission: ResourceAdmissionConfig,
}

impl AdmissionConfig {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("invalid admission config: {e}"))
    }

    pub fn acknowledgement(&self) -> String {
        format!(
            "v1:{}",
            serde_json::to_string(self).expect("admission policy serializes")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_secure_defaults() {
        let config = AdmissionConfig::parse("{}").unwrap();
        assert!(!config.allow_driver_config);
        assert!(config.resource_admission.enabled);
        assert_eq!(
            config.acknowledgement(),
            r#"v1:{"allow_driver_config":false,"resource_admission":{"enabled":true,"required_labels":{"openshell.ai/sandbox-attachable":"true","openshell.ai/sandbox-attachable-workspace":"${workspace}"}}}"#
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(AdmissionConfig::parse(r#"{"allow_everything":true}"#).is_err());
    }
}
