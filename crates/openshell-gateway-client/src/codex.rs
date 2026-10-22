//! Codex accounts. Codex signs in with ChatGPT rather than an API key, so an
//! account is imported from a Codex sign-in on the host (`codex login`
//! writes it to `~/.codex/auth.json`). The gateway keeps the refresh token
//! and renews the access token itself; sandboxes only get opaque handles for
//! the access token and account id, which the proxy swaps for the real
//! values on requests to OpenAI.

use std::collections::BTreeMap;

use openshell_core::proto::{
    ConfigureProviderRefreshRequest, ProviderCredentialRefreshStrategy,
    RotateProviderCredentialRequest,
};
use serde::Deserialize;

use crate::{Error, Gateway, ProviderInfo, workspace};

/// The provider profile Codex accounts use.
pub const PROFILE: &str = "codex";
pub const ACCESS_TOKEN: &str = "CODEX_AUTH_ACCESS_TOKEN";
pub const ACCOUNT_ID: &str = "CODEX_AUTH_ACCOUNT_ID";
/// The Codex CLI's public OAuth client, which its refresh tokens belong to.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The tokens from a Codex sign-in.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

impl std::fmt::Debug for CodexTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexTokens")
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    account_id: String,
}

impl CodexTokens {
    /// Reads the contents of Codex's `auth.json`. The message says what to
    /// do when it holds no ChatGPT sign-in.
    pub fn from_auth_json(json: &str) -> Result<Self, String> {
        let file: AuthFile = serde_json::from_str(json)
            .map_err(|_| "This isn't a Codex sign-in file (auth.json).".to_owned())?;
        let Some(tokens) = file.tokens else {
            return Err(
                "This Codex sign-in uses an API key, not a ChatGPT account. \
                 Run \"codex login\" and sign in with ChatGPT."
                    .to_owned(),
            );
        };
        if tokens.access_token.is_empty()
            || tokens.refresh_token.is_empty()
            || tokens.account_id.is_empty()
        {
            return Err("This Codex sign-in is incomplete. Run \"codex login\" again.".to_owned());
        }
        Ok(Self {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id: tokens.account_id,
        })
    }
}

impl Gateway {
    /// Stores `tokens` as Codex provider `name`, hands the refresh token to
    /// the gateway's refresher, and renews the access token once so the
    /// account is known to work. Removes the provider again if that fails.
    pub async fn create_codex_provider(
        &mut self,
        name: &str,
        tokens: &CodexTokens,
    ) -> Result<ProviderInfo, Error> {
        let credentials = BTreeMap::from([
            (ACCESS_TOKEN.to_owned(), tokens.access_token.clone()),
            (ACCOUNT_ID.to_owned(), tokens.account_id.clone()),
        ]);
        let provider = self.create_provider(name, PROFILE, credentials).await?;
        if let Err(error) = self.configure_codex_refresh(name, tokens).await {
            let _ = self.delete_provider(name).await;
            return Err(error);
        }
        Ok(provider)
    }

    async fn configure_codex_refresh(
        &mut self,
        name: &str,
        tokens: &CodexTokens,
    ) -> Result<(), Error> {
        self.client
            .configure_provider_refresh(ConfigureProviderRefreshRequest {
                workspace_scope: workspace(),
                provider: name.into(),
                credential_key: ACCESS_TOKEN.into(),
                strategy: ProviderCredentialRefreshStrategy::Oauth2RefreshToken as i32,
                material: [
                    ("client_id".to_owned(), CLIENT_ID.to_owned()),
                    ("refresh_token".to_owned(), tokens.refresh_token.clone()),
                ]
                .into(),
                secret_material_keys: vec!["refresh_token".into()],
                ..Default::default()
            })
            .await?;
        self.client
            .rotate_provider_credential(RotateProviderCredentialRequest {
                workspace_scope: workspace(),
                provider: name.into(),
                credential_key: ACCESS_TOKEN.into(),
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
    fn reads_a_chatgpt_sign_in() {
        let json = r#"{
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "x.y.z",
                "access_token": "at",
                "refresh_token": "secret-refresh",
                "account_id": "acct"
            },
            "last_refresh": "2026-10-01T00:00:00Z"
        }"#;
        let tokens = CodexTokens::from_auth_json(json).unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "secret-refresh");
        assert_eq!(tokens.account_id, "acct");
        assert!(!format!("{tokens:?}").contains("secret"));
    }

    #[test]
    fn explains_unusable_files() {
        assert!(CodexTokens::from_auth_json("not json").is_err());
        let api_key = r#"{"OPENAI_API_KEY": "sk-1", "tokens": null}"#;
        assert!(
            CodexTokens::from_auth_json(api_key)
                .unwrap_err()
                .contains("API key")
        );
        let partial = r#"{"tokens": {"access_token": "at"}}"#;
        assert!(CodexTokens::from_auth_json(partial).is_err());
    }
}
