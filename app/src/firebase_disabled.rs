#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirebaseError {
    pub code: i32,
    pub message: String,
}

impl std::error::Error for FirebaseError {}

impl std::fmt::Display for FirebaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Firebase request failed with status {} and message: {}",
            self.code, self.message
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FetchAccessTokenResponse {
    Success {
        #[serde(alias = "expiresIn")]
        expires_in: String,
        #[serde(alias = "idToken")]
        id_token: String,
        #[serde(alias = "refreshToken")]
        refresh_token: String,
    },
    Error {
        error: FirebaseError,
    },
}
