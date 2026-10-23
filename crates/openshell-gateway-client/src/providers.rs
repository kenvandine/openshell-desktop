//! Provider profiles and providers: how sandboxes get credentials.
//!
//! OpenShell no longer ships built-in provider profiles, so a fresh gateway
//! knows no provider types at all. The app bundles the upstream example
//! profiles it offers and imports any the gateway is missing. A provider is
//! one stored set of credentials of a profile's type; the gateway keeps the
//! secret and the supervisor injects it only into requests to the profile's
//! endpoints.

use std::collections::{BTreeMap, BTreeSet};

use openshell_core::proto::datamodel::v1::{ObjectMeta, Provider};
use openshell_core::proto::{
    CreateProviderRequest, DeleteProviderRequest, ImportProviderProfilesRequest,
    ListProviderProfilesRequest, ListProvidersRequest, ProviderProfile, ProviderProfileImportItem,
    UpdateProviderProfilesRequest,
};

use crate::{Error, Gateway, workspace};

/// Upstream example profiles (NVIDIA/OpenShell `providers/`, same revision as
/// the gateway), by file name.
const BUNDLED: &[(&str, &str)] = &[
    ("anthropic.yaml", include_str!("../profiles/anthropic.yaml")),
    (
        "claude-code.yaml",
        include_str!("../profiles/claude-code.yaml"),
    ),
    ("codex.yaml", include_str!("../profiles/codex.yaml")),
    ("copilot.yaml", include_str!("../profiles/copilot.yaml")),
    ("github.yaml", include_str!("../profiles/github.yaml")),
    ("openai.yaml", include_str!("../profiles/openai.yaml")),
    (
        "openrouter.yaml",
        include_str!("../profiles/openrouter.yaml"),
    ),
];

/// The bundled profiles, parsed. They are compiled in, so a parse failure is
/// a packaging bug, not a runtime condition.
pub fn bundled_profiles() -> Vec<ProviderProfile> {
    BUNDLED
        .iter()
        .map(|(file, yaml)| {
            openshell_providers::parse_profile_yaml(yaml)
                .unwrap_or_else(|e| panic!("bundled profile {file}: {e}"))
                .to_proto()
        })
        .collect()
}

/// One credential a profile asks for, as a form would show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialField {
    pub name: String,
    pub description: String,
    /// The first is the key the gateway stores the value under.
    pub env_vars: Vec<String>,
    pub required: bool,
}

/// A provider type the gateway knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub credentials: Vec<CredentialField>,
    /// Hosts the credential may be sent to.
    pub hosts: Vec<String>,
}

impl ProfileInfo {
    /// Whether one pasted key is all it needs. Profiles that want several
    /// values, such as Codex's sign-in tokens, need a sign-in flow instead.
    pub fn takes_single_key(&self) -> bool {
        self.credentials.iter().filter(|c| c.required).count() <= 1 && !self.credentials.is_empty()
    }

    pub fn from_proto(profile: &ProviderProfile) -> Self {
        let mut hosts: Vec<String> = Vec::new();
        for endpoint in &profile.endpoints {
            if !hosts.contains(&endpoint.host) {
                hosts.push(endpoint.host.clone());
            }
        }
        Self {
            id: profile.id.clone(),
            display_name: profile.display_name.clone(),
            description: profile.description.clone(),
            credentials: profile
                .credentials
                .iter()
                .map(|c| CredentialField {
                    name: c.name.clone(),
                    description: c.description.clone(),
                    env_vars: c.env_vars.clone(),
                    required: c.required,
                })
                .collect(),
            hosts,
        }
    }
}

/// A stored provider. Its secret values never leave the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: String,
    /// The profile id it was created from.
    pub kind: String,
    /// Which credential keys hold a value.
    pub credential_keys: Vec<String>,
    /// Its non-secret settings.
    pub config: BTreeMap<String, String>,
}

impl ProviderInfo {
    pub fn from_proto(provider: &Provider) -> Self {
        let mut credential_keys: Vec<String> = provider.credentials.keys().cloned().collect();
        credential_keys.extend(provider.credential_handles.keys().cloned());
        credential_keys.sort();
        credential_keys.dedup();
        Self {
            name: provider
                .metadata
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_default(),
            kind: provider.r#type.clone(),
            credential_keys,
            config: provider.config.clone().into_iter().collect(),
        }
    }
}

/// Bundled profiles whose ids are not in `existing`.
fn missing(existing: &BTreeSet<String>) -> Vec<ProviderProfileImportItem> {
    BUNDLED
        .iter()
        .zip(bundled_profiles())
        .filter(|(_, profile)| !existing.contains(&profile.id))
        .map(|((file, _), profile)| ProviderProfileImportItem {
            profile: Some(profile),
            source: format!("openshell-desktop/{file}"),
        })
        .collect()
}

/// Bundled profiles that let the gateway renew a credential, where the
/// gateway's copy of that profile does not. Earlier versions of the app
/// imported upstream's static Codex profile, and Codex sign-in needs the
/// renewing one. Each comes with the stored profile's resource version.
fn stale(existing: &[ProviderProfile]) -> Vec<(ProviderProfile, u64)> {
    bundled_profiles()
        .into_iter()
        .filter_map(|bundled| {
            let stored = existing.iter().find(|p| p.id == bundled.id)?;
            let lacks_refresh = bundled.credentials.iter().any(|credential| {
                credential.refresh.is_some()
                    && stored
                        .credentials
                        .iter()
                        .any(|c| c.name == credential.name && c.refresh.is_none())
            });
            lacks_refresh.then_some((bundled, stored.resource_version))
        })
        .collect()
}

impl Gateway {
    /// Platform-scope provider profiles (the scope `profile import --global`
    /// uses), across every page.
    pub async fn profiles(&mut self) -> Result<Vec<ProviderProfile>, Error> {
        let mut profiles = Vec::new();
        let mut page_token = String::new();
        loop {
            let response = self
                .client
                .list_provider_profiles(ListProviderProfilesRequest {
                    workspace_scope: None,
                    page_size: 0,
                    page_token,
                })
                .await?
                .into_inner();
            profiles.extend(response.profiles);
            if response.next_page_token.is_empty() {
                return Ok(profiles);
            }
            page_token = response.next_page_token;
        }
    }

    /// Imports the bundled profiles the gateway does not have yet, at
    /// platform scope. Profiles the user changed are left alone, since import
    /// only creates. Returns how many were imported.
    pub async fn ensure_profiles(&mut self) -> Result<usize, Error> {
        let profiles = self.profiles().await?;
        for (profile, version) in stale(&profiles) {
            let id = profile.id.clone();
            self.client
                .update_provider_profiles(UpdateProviderProfilesRequest {
                    workspace_scope: None,
                    profile: Some(ProviderProfileImportItem {
                        profile: Some(profile),
                        source: format!("openshell-desktop/{id}.yaml"),
                    }),
                    expected_resource_version: version,
                    id,
                    request_id: String::new(),
                })
                .await?;
        }
        let existing = profiles.into_iter().map(|p| p.id).collect();
        let items = missing(&existing);
        if items.is_empty() {
            return Ok(0);
        }
        let response = self
            .client
            .import_provider_profiles(ImportProviderProfilesRequest {
                workspace_scope: None,
                profiles: items,
                request_id: String::new(),
            })
            .await?
            .into_inner();
        if !response.imported {
            let problems: Vec<String> = response
                .diagnostics
                .iter()
                .map(|d| format!("{} {}: {}", d.profile_id, d.field, d.message))
                .collect();
            return Err(Error::Rpc(tonic::Status::invalid_argument(format!(
                "provider profiles rejected: {}",
                problems.join("; ")
            ))));
        }
        Ok(response.profiles.len())
    }

    pub async fn providers(&mut self) -> Result<Vec<ProviderInfo>, Error> {
        let mut providers = Vec::new();
        let mut page_token = String::new();
        loop {
            let response = self
                .client
                .list_providers(ListProvidersRequest {
                    workspace_scope: workspace(),
                    page_size: 0,
                    page_token,
                    label_selector: String::new(),
                })
                .await?
                .into_inner();
            providers.extend(response.providers.iter().map(ProviderInfo::from_proto));
            if response.next_page_token.is_empty() {
                return Ok(providers);
            }
            page_token = response.next_page_token;
        }
    }

    /// Stores `credentials` (env var name → secret) as provider `name` of
    /// profile `kind`.
    pub async fn create_provider(
        &mut self,
        name: &str,
        kind: &str,
        credentials: BTreeMap<String, String>,
        config: BTreeMap<String, String>,
    ) -> Result<ProviderInfo, Error> {
        let response = self
            .client
            .create_provider(CreateProviderRequest {
                workspace_scope: workspace(),
                provider: Some(Provider {
                    metadata: Some(ObjectMeta {
                        name: name.into(),
                        ..Default::default()
                    }),
                    r#type: kind.into(),
                    credentials: credentials.into_iter().collect(),
                    config: config.into_iter().collect(),
                    ..Default::default()
                }),
                request_id: String::new(),
            })
            .await?
            .into_inner();
        Ok(response
            .provider
            .as_ref()
            .map(ProviderInfo::from_proto)
            .unwrap_or_else(|| ProviderInfo {
                name: name.into(),
                kind: kind.into(),
                credential_keys: Vec::new(),
                config: BTreeMap::new(),
            }))
    }

    pub async fn delete_provider(&mut self, name: &str) -> Result<(), Error> {
        self.client
            .delete_provider(DeleteProviderRequest {
                workspace_scope: workspace(),
                name: name.into(),
                allow_missing: true,
                request_id: String::new(),
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_profiles_parse() {
        let ids: Vec<String> = bundled_profiles().into_iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "anthropic",
                "claude-code",
                "codex",
                "copilot",
                "github",
                "openai",
                "openrouter"
            ]
        );
    }

    #[test]
    fn imports_only_missing_profiles() {
        let existing: BTreeSet<String> = ["claude-code".to_owned(), "github".to_owned()].into();
        let ids: Vec<String> = missing(&existing)
            .into_iter()
            .map(|item| item.profile.unwrap().id)
            .collect();
        assert!(!ids.contains(&"claude-code".to_owned()));
        assert!(!ids.contains(&"github".to_owned()));
        assert_eq!(ids.len(), BUNDLED.len() - 2);
    }

    #[test]
    fn upgrades_profiles_that_cannot_renew_credentials() {
        let mut stored = bundled_profiles();
        assert!(stale(&stored).is_empty());
        let codex = stored.iter_mut().find(|p| p.id == "codex").unwrap();
        codex.resource_version = 7;
        for credential in &mut codex.credentials {
            credential.refresh = None;
        }
        let upgrades = stale(&stored);
        assert_eq!(upgrades.len(), 1);
        assert_eq!(upgrades[0].0.id, "codex");
        assert_eq!(upgrades[0].1, 7);
    }

    #[test]
    fn profile_info_lists_credentials_and_unique_hosts() {
        let profile = bundled_profiles()
            .into_iter()
            .find(|p| p.id == "github")
            .unwrap();
        let info = ProfileInfo::from_proto(&profile);
        assert_eq!(info.display_name, "GitHub");
        assert_eq!(info.credentials[0].env_vars, ["GITHUB_TOKEN", "GH_TOKEN"]);
        assert_eq!(info.hosts, ["api.github.com", "github.com"]);
    }

    #[test]
    fn only_single_key_profiles_can_be_pasted() {
        let single: Vec<String> = bundled_profiles()
            .iter()
            .map(ProfileInfo::from_proto)
            .filter(ProfileInfo::takes_single_key)
            .map(|p| p.id)
            .collect();
        assert!(single.contains(&"claude-code".to_owned()));
        assert!(single.contains(&"github".to_owned()));
        assert!(!single.contains(&"codex".to_owned()));
    }
}
