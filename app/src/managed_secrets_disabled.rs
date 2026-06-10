#![allow(dead_code, unused_imports)]

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use warp_graphql::managed_secrets::{ManagedSecret, ManagedSecretType};
use warpui::{Entity, SingletonEntity};

fn unavailable() -> anyhow::Error {
    anyhow::anyhow!("Warp-managed secrets are not available in this build")
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum ManagedSecretValue {
    RawValue {
        value: String,
    },
    AnthropicApiKey {
        api_key: String,
    },
    AnthropicBedrockAccessKey {
        aws_access_key_id: String,
        aws_secret_access_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        aws_session_token: Option<String>,
        aws_region: String,
    },
    AnthropicBedrockApiKey {
        aws_bearer_token_bedrock: String,
        aws_region: String,
    },
    OpenaiApiKey {
        api_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

impl ManagedSecretValue {
    pub fn raw_value(s: impl Into<String>) -> Self {
        Self::RawValue { value: s.into() }
    }

    pub fn anthropic_api_key(s: impl Into<String>) -> Self {
        Self::AnthropicApiKey { api_key: s.into() }
    }

    pub fn anthropic_bedrock_access_key(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        region: impl Into<String>,
    ) -> Self {
        Self::AnthropicBedrockAccessKey {
            aws_access_key_id: access_key_id.into(),
            aws_secret_access_key: secret_access_key.into(),
            aws_session_token: session_token,
            aws_region: region.into(),
        }
    }

    pub fn anthropic_bedrock_api_key(token: impl Into<String>, region: impl Into<String>) -> Self {
        Self::AnthropicBedrockApiKey {
            aws_bearer_token_bedrock: token.into(),
            aws_region: region.into(),
        }
    }

    pub fn openai_api_key(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self::OpenaiApiKey {
            api_key: api_key.into(),
            base_url,
        }
    }

    pub fn secret_type(&self) -> ManagedSecretType {
        match self {
            Self::RawValue { .. } => ManagedSecretType::RawValue,
            Self::AnthropicApiKey { .. } => ManagedSecretType::AnthropicApiKey,
            Self::AnthropicBedrockAccessKey { .. } => ManagedSecretType::AnthropicBedrockAccessKey,
            Self::AnthropicBedrockApiKey { .. } => ManagedSecretType::AnthropicBedrockApiKey,
            Self::OpenaiApiKey { .. } => ManagedSecretType::OpenaiApiKey,
        }
    }
}

impl std::fmt::Debug for ManagedSecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RawValue { .. } => f
                .debug_struct("ManagedSecret::RawValue")
                .finish_non_exhaustive(),
            Self::AnthropicApiKey { .. } => f
                .debug_struct("ManagedSecret::AnthropicApiKey")
                .finish_non_exhaustive(),
            Self::AnthropicBedrockAccessKey { .. } => f
                .debug_struct("ManagedSecret::AnthropicBedrockAccessKey")
                .finish_non_exhaustive(),
            Self::AnthropicBedrockApiKey { .. } => f
                .debug_struct("ManagedSecret::AnthropicBedrockApiKey")
                .finish_non_exhaustive(),
            Self::OpenaiApiKey { .. } => f
                .debug_struct("ManagedSecret::OpenaiApiKey")
                .finish_non_exhaustive(),
        }
    }
}

pub mod client {
    use super::*;
    use async_trait::async_trait;
    use vec1::Vec1;

    pub use warp_graphql::queries::task_secrets::ManagedSecretValue;

    #[derive(Debug, Clone)]
    pub struct TaskIdentityToken {
        pub token: String,
        pub expires_at: DateTime<Utc>,
        pub issuer: String,
    }

    pub struct IdentityTokenOptions {
        pub audience: String,
        pub requested_duration: Duration,
        pub subject_template: Vec1<String>,
    }

    #[derive(Debug)]
    pub struct ManagedSecretConfigs {
        pub user_secrets: Option<warp_graphql::managed_secrets::ManagedSecretConfig>,
        pub team_secrets: HashMap<String, warp_graphql::managed_secrets::ManagedSecretConfig>,
    }

    #[derive(Debug, Clone)]
    pub enum SecretOwner {
        CurrentUser,
        Team { team_uid: String },
    }

    #[cfg_attr(not(target_family = "wasm"), async_trait)]
    #[cfg_attr(target_family = "wasm", async_trait(?Send))]
    pub trait ManagedSecretsClient: 'static + Send + Sync {
        async fn get_managed_secret_configs(&self) -> anyhow::Result<ManagedSecretConfigs>;

        async fn create_managed_secret(
            &self,
            owner: SecretOwner,
            name: String,
            secret_type: ManagedSecretType,
            encrypted_value: String,
            description: Option<String>,
        ) -> anyhow::Result<ManagedSecret>;

        async fn delete_managed_secret(
            &self,
            owner: SecretOwner,
            name: String,
        ) -> anyhow::Result<()>;

        async fn update_managed_secret(
            &self,
            owner: SecretOwner,
            name: String,
            encrypted_value: Option<String>,
            description: Option<String>,
        ) -> anyhow::Result<ManagedSecret>;

        async fn list_secrets(&self) -> anyhow::Result<Vec<ManagedSecret>>;

        async fn list_harness_auth_secrets(
            &self,
            harness: warp_graphql::ai::AgentHarness,
        ) -> anyhow::Result<Vec<ManagedSecret>>;

        async fn get_task_secrets(
            &self,
            task_id: String,
            workload_token: String,
        ) -> anyhow::Result<HashMap<String, ManagedSecretValue>>;

        async fn issue_task_identity_token(
            &self,
            options: IdentityTokenOptions,
        ) -> anyhow::Result<TaskIdentityToken>;
    }
}

pub use client::TaskIdentityToken;

pub struct UploadKey;

pub fn init_envelope() {}

#[derive(Debug, Clone)]
pub struct GcpFederationConfig {
    pub project_number: String,
    pub pool_id: String,
    pub provider_id: String,
    pub service_account_email: Option<String>,
    pub token_lifetime: Option<Duration>,
}

pub struct GcpCredentials;

impl GcpCredentials {
    pub fn federated(
        _task_id: &str,
        _config: &GcpFederationConfig,
    ) -> Result<Self, PrepareGcpCredentialsError> {
        Err(PrepareGcpCredentialsError::Unavailable)
    }

    pub fn env_vars(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        HashMap::new()
    }

    pub fn cleanup(self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareGcpCredentialsError {
    #[error("GCP workload identity federation is not available in this build")]
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GcpWorkloadIdentityFederationToken {
    pub version: u8,
    pub success: bool,
    pub token_type: String,
    pub id_token: String,
    pub expiration_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GcpWorkloadIdentityFederationError {
    pub version: u8,
    pub success: bool,
    pub code: String,
    pub message: String,
}

impl GcpWorkloadIdentityFederationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            version: 1,
            success: false,
            code: "TOKEN_ISSUANCE_FAILED".into(),
            message: message.into(),
        }
    }
}

pub trait ActorProvider: Send + Sync + 'static {
    fn actor_uid(&self) -> Option<String>;
}

pub struct ManagedSecretManager {
    client: Arc<dyn client::ManagedSecretsClient>,
    _actor_provider: Arc<dyn ActorProvider>,
}

impl ManagedSecretManager {
    pub fn new(
        client: Arc<dyn client::ManagedSecretsClient>,
        actor_provider: Arc<dyn ActorProvider>,
    ) -> Self {
        Self {
            client,
            _actor_provider: actor_provider,
        }
    }

    pub fn create_secret(
        &self,
        _owner: client::SecretOwner,
        _name: String,
        _value: ManagedSecretValue,
        _description: Option<String>,
    ) -> impl Future<Output = anyhow::Result<ManagedSecret>> + use<> {
        async { Err(unavailable()) }
    }

    pub fn delete_secret(
        &self,
        _owner: client::SecretOwner,
        _name: String,
    ) -> impl Future<Output = anyhow::Result<()>> + use<> {
        async { Err(unavailable()) }
    }

    pub fn update_secret(
        &self,
        _owner: client::SecretOwner,
        _name: String,
        _value: Option<ManagedSecretValue>,
        _description: Option<String>,
    ) -> impl Future<Output = anyhow::Result<ManagedSecret>> + use<> {
        async { Err(unavailable()) }
    }

    pub fn list_secrets(&self) -> impl Future<Output = anyhow::Result<Vec<ManagedSecret>>> + use<> {
        async { Ok(Vec::new()) }
    }

    pub fn get_task_secrets(
        &self,
        _task_id: String,
    ) -> impl Future<Output = anyhow::Result<HashMap<String, ManagedSecretValue>>> + use<> {
        async { Ok(HashMap::new()) }
    }

    pub fn issue_task_identity_token(
        &self,
        _options: client::IdentityTokenOptions,
    ) -> impl Future<Output = anyhow::Result<client::TaskIdentityToken>> + use<> {
        async { Err(unavailable()) }
    }

    pub fn issue_gcp_workload_identity_federation_token(
        &self,
        _audience: String,
        _token_type: String,
        _requested_duration: Duration,
    ) -> impl Future<
        Output = Result<GcpWorkloadIdentityFederationToken, GcpWorkloadIdentityFederationError>,
    > + use<> {
        async {
            Err(GcpWorkloadIdentityFederationError::new(
                unavailable().to_string(),
            ))
        }
    }
}

impl Entity for ManagedSecretManager {
    type Event = ();
}

impl SingletonEntity for ManagedSecretManager {}
