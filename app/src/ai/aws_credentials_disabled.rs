#![allow(dead_code)]

pub use ai::api_keys::AwsCredentials;
use ai::api_keys::{ApiKeyManager, AwsCredentialsState};
use futures::future::BoxFuture;
use warpui::{ModelContext, ModelHandle};

use crate::terminal::model_events::ModelEventDispatcher;

#[derive(Debug, Clone)]
pub enum LoadAwsCredentialsError {
    NotConfigured,
    CredentialsLoadFailed(String),
}

impl std::fmt::Display for LoadAwsCredentialsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "AWS credentials are not available in this build"),
            Self::CredentialsLoadFailed(message) => {
                write!(f, "Failed to load AWS credentials: {message}")
            }
        }
    }
}

impl std::error::Error for LoadAwsCredentialsError {}

pub(crate) fn aws_role_session_name(run_id: &str) -> String {
    format!("Oz_Run_{run_id}")
}

pub async fn load_aws_credentials_from_sdk(
    _profile: &str,
) -> Result<AwsCredentials, LoadAwsCredentialsError> {
    Err(LoadAwsCredentialsError::NotConfigured)
}

pub trait AwsCredentialRefresher {
    fn register_model_event_dispatcher(
        &mut self,
        _model_events: &ModelHandle<ModelEventDispatcher>,
        _ctx: &mut ModelContext<Self>,
    ) where
        Self: Sized;

    fn subscribe_to_settings_changes(&mut self, _ctx: &mut ModelContext<Self>)
    where
        Self: Sized;
}

impl AwsCredentialRefresher for ApiKeyManager {
    fn register_model_event_dispatcher(
        &mut self,
        _model_events: &ModelHandle<ModelEventDispatcher>,
        _ctx: &mut ModelContext<Self>,
    ) {
    }

    fn subscribe_to_settings_changes(&mut self, _ctx: &mut ModelContext<Self>) {}
}

pub(crate) fn refresh_aws_credentials(
    manager: &mut ApiKeyManager,
    ctx: &mut ModelContext<ApiKeyManager>,
) -> BoxFuture<'static, Result<(), String>> {
    manager.set_aws_credentials_state(AwsCredentialsState::Disabled, ctx);
    Box::pin(async { Ok(()) })
}
