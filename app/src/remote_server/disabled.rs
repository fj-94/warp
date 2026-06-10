#![allow(dead_code, unused_imports)]

use std::sync::Arc;

use serde::Serialize;
use warp_core::{HostId, SessionId};
use warp_util::remote_path::RemotePath;
use warp_util::standardized_path::StandardizedPath;
use warpui::{Entity, ModelContext, SingletonEntity};

pub mod setup {
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub enum UnsupportedReason {
        UnsupportedOs { os: String },
        GlibcTooOld { version: String },
        NonGlibc { libc: String },
        UnsupportedArch { arch: String },
    }

    impl UnsupportedReason {
        pub fn as_telemetry_reason(&self) -> &'static str {
            match self {
                Self::UnsupportedOs { .. } => "unsupported_os",
                Self::GlibcTooOld { .. } => "glibc_too_old",
                Self::NonGlibc { .. } => "non_glibc",
                Self::UnsupportedArch { .. } => "unsupported_arch",
            }
        }
    }

    pub fn remote_server_daemon_data_dir(_identity_key: &str) -> String {
        "~/.warp/remote_server_disabled".to_string()
    }
}

pub mod transport {
    use serde::Serialize;

    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum InstallSource {
        Server,
        Client,
    }

    #[derive(Clone, Copy, Debug)]
    pub enum SetupStage {
        DetectPlatform,
        PreinstallCheck,
        InstallBinary,
        CheckBinary,
        Launch,
    }

    #[derive(Clone, Debug)]
    pub struct UserFacingError {
        pub body: String,
        pub detail: Option<String>,
    }

    #[derive(Clone, Debug)]
    pub struct Error;

    impl Error {
        pub fn user_facing_error(&self, _stage: SetupStage) -> UserFacingError {
            UserFacingError {
                body: "Remote server is not available in this build".to_string(),
                detail: None,
            }
        }
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("remote server is not available in this build")
        }
    }

    impl std::error::Error for Error {}
}

pub mod codebase_index_proto {
    use serde::Serialize;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RemoteCodebaseIndexState {
        NotEnabled,
        Unavailable,
        Disabled,
        Queued,
        Indexing,
        Ready,
        Stale,
        Failed,
    }

    #[derive(Clone, Debug)]
    pub struct RemoteCodebaseIndexStatus {
        pub repo_path: String,
        pub state: RemoteCodebaseIndexState,
        pub last_updated_epoch_millis: Option<u64>,
        pub progress_completed: Option<u64>,
        pub progress_total: Option<u64>,
        pub failure_message: Option<String>,
        pub root_hash: Option<String>,
    }
}

pub mod proto {
    #[derive(Clone, Debug)]
    pub struct TextEdit {
        pub start_offset: u64,
        pub end_offset: u64,
        pub text: String,
    }

    #[derive(Clone, Debug)]
    pub struct ReadFileContextRequest {
        pub files: Vec<ReadFileContextFile>,
        pub max_file_bytes: Option<u32>,
        pub max_batch_bytes: Option<u32>,
    }

    #[derive(Clone, Debug)]
    pub struct ReadFileContextFile {
        pub path: String,
        pub line_ranges: Vec<LineRange>,
    }

    #[derive(Clone, Debug)]
    pub struct LineRange {
        pub start: u32,
        pub end: u32,
    }

    #[derive(Clone, Debug)]
    pub struct ReadFileContextResponse {
        pub file_contexts: Vec<FileContextProto>,
        pub failed_files: Vec<FailedFileRead>,
    }

    #[derive(Clone, Debug)]
    pub struct FailedFileRead {
        pub path: String,
        pub error: Option<FileOperationError>,
    }

    #[derive(Clone, Debug)]
    pub struct FileContextProto {
        pub file_name: String,
        pub content: Option<file_context_proto::Content>,
        pub line_range_start: Option<u32>,
        pub line_range_end: Option<u32>,
        pub last_modified_epoch_millis: Option<u64>,
        pub line_count: u32,
    }

    pub mod file_context_proto {
        #[derive(Clone, Debug)]
        pub enum Content {
            TextContent(String),
            BinaryContent(Vec<u8>),
        }
    }

    #[derive(Clone, Debug)]
    pub struct OpenBufferResponse {
        pub result: Option<open_buffer_response::Result>,
    }

    pub mod open_buffer_response {
        #[derive(Clone, Debug)]
        pub enum Result {
            Success(super::OpenBufferSuccess),
            Error(super::FileOperationError),
        }
    }

    #[derive(Clone, Debug)]
    pub struct OpenBufferSuccess {
        pub content: String,
        pub server_version: u64,
    }

    #[derive(Clone, Debug)]
    pub struct FileOperationError {
        pub message: String,
    }
}

pub mod client {
    use super::proto;

    #[derive(Debug)]
    pub struct RemoteServerClient;

    impl RemoteServerClient {
        pub async fn save_buffer(&self, _path: String) -> Result<(), String> {
            Err("remote server is not available in this build".to_string())
        }

        pub async fn open_buffer(
            &self,
            _path: String,
            _fetch_base: bool,
        ) -> Result<proto::OpenBufferResponse, String> {
            Err("remote server is not available in this build".to_string())
        }

        pub fn send_buffer_edit(
            &self,
            _path: String,
            _expected_server_version: u64,
            _client_version: u64,
            _edits: Vec<proto::TextEdit>,
        ) {
        }

        pub async fn read_file_context(
            &self,
            _request: proto::ReadFileContextRequest,
        ) -> Result<proto::ReadFileContextResponse, String> {
            Err("remote server is not available in this build".to_string())
        }
    }
}

pub mod manager {
    use super::*;

    pub const MAX_RECONNECT_ATTEMPTS: u32 = 0;

    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RemoteServerInitPhase {
        Connect,
        Initialize,
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RemoteServerOperation {
        NavigateToDirectory,
        LoadRepoMetadataDirectory,
        IndexCodebase,
        ResyncCodebase,
        DropCodebaseIndex,
        OpenBuffer,
        SaveBuffer,
        WriteFile,
        ReadFileContext,
        DeleteFile,
        RunCommand,
        GetFragmentMetadataFromHash,
        GetDiffState,
        DiscardFiles,
        GetBranches,
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RemoteServerErrorKind {
        Timeout,
        Disconnected,
        ServerError,
        Other,
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RemoteCodebaseIndexUpdateOperation {
        IndexNewRepo { is_auto_index: bool },
        Sync { is_full_sync: bool },
        Drop,
    }

    #[derive(Clone, Debug)]
    pub struct RemotePlatform {
        pub os: String,
        pub arch: String,
    }

    #[derive(Clone, Debug, Serialize)]
    pub struct RemoteServerExitStatus {
        pub code: Option<i32>,
        pub signal_killed: bool,
    }

    #[derive(Clone, Debug)]
    pub enum RemoteServerManagerEvent {
        SessionConnected {
            session_id: SessionId,
            host_id: HostId,
        },
        SessionReconnected {
            session_id: SessionId,
            client: Arc<client::RemoteServerClient>,
        },
        SessionDisconnected {
            session_id: SessionId,
            exit_status: Option<RemoteServerExitStatus>,
            was_reconnect_attempt: bool,
        },
        SessionDeregistered {
            session_id: SessionId,
        },
        SetupStateChanged {
            session_id: SessionId,
            state: crate::terminal::event::RemoteServerSetupState,
        },
        HostConnected {
            host_id: HostId,
        },
        HostDisconnected {
            host_id: HostId,
        },
        NavigatedToDirectory {
            session_id: SessionId,
        },
        RepoMetadataSnapshot {
            host_id: HostId,
            update: repo_metadata::RepositoryUpdate,
        },
        RepoMetadataUpdated {
            host_id: HostId,
            update: repo_metadata::RepositoryUpdate,
        },
        RepoMetadataDirectoryLoaded {
            host_id: HostId,
            update: repo_metadata::RepositoryUpdate,
        },
        BufferUpdated {},
        BufferConflictDetected {},
        SessionConnecting {
            session_id: SessionId,
        },
        SessionConnectionFailed {
            session_id: SessionId,
        },
        CodebaseIndexStatusesSnapshot {},
        CodebaseIndexStatusUpdated {},
        CodebaseIndexMutationFailed {},
        BinaryCheckComplete {},
        BinaryInstallComplete {},
        ClientRequestFailed {},
        ServerMessageDecodingError {},
        DiffStateSnapshotReceived {},
        DiffStateMetadataUpdateReceived {},
        DiffStateFileDeltaReceived {},
        GetBranchesResponse {},
    }

    impl RemoteServerManagerEvent {
        pub fn session_id(&self) -> Option<&SessionId> {
            match self {
                Self::SessionConnected { session_id, .. }
                | Self::SessionReconnected { session_id, .. }
                | Self::SessionDisconnected { session_id, .. }
                | Self::SessionDeregistered { session_id }
                | Self::SetupStateChanged { session_id, .. }
                | Self::SessionConnecting { session_id }
                | Self::SessionConnectionFailed { session_id } => Some(session_id),
                _ => None,
            }
        }
    }

    pub struct RemoteServerManager;

    impl RemoteServerManager {
        pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
            Self
        }

        pub fn client_for_host(
            &self,
            _host_id: &HostId,
        ) -> Option<&Arc<client::RemoteServerClient>> {
            None
        }

        pub fn client_for_session(
            &self,
            _session_id: SessionId,
        ) -> Option<&Arc<client::RemoteServerClient>> {
            None
        }

        pub fn host_id_for_session(&self, _session_id: SessionId) -> Option<&HostId> {
            None
        }

        pub fn platform_for_session(&self, _session_id: SessionId) -> Option<&RemotePlatform> {
            None
        }

        pub fn is_session_potentially_active(&self, _session_id: SessionId) -> bool {
            false
        }

        pub fn find_connected_session(&self, _host_id: &HostId) -> Option<SessionId> {
            None
        }

        pub fn notify_session_bootstrapped(
            &mut self,
            _session_id: SessionId,
            _shell_type_name: &str,
            _shell_path: Option<&str>,
        ) {
        }

        pub fn navigate_to_directory(
            &mut self,
            _session_id: SessionId,
            _directory: String,
            _ctx: &mut ModelContext<Self>,
        ) {
        }

        pub fn load_remote_repo_metadata_directory(
            &mut self,
            _session_id: SessionId,
            _repo_path: String,
            _dir_path: String,
            _ctx: &mut ModelContext<Self>,
        ) {
        }
    }

    impl Entity for RemoteServerManager {
        type Event = RemoteServerManagerEvent;
    }

    impl SingletonEntity for RemoteServerManager {}
}

pub mod codebase_index_model {
    use super::*;

    #[derive(Clone, Debug)]
    pub struct RemoteCodebaseIndexSettingsEntry {
        pub host_label: String,
        pub remote_path: RemotePath,
        pub status: codebase_index_proto::RemoteCodebaseIndexStatus,
    }

    #[derive(Clone, Debug)]
    pub enum RemoteCodebaseIndexModelEvent {
        SettingsEntriesChanged,
    }

    #[derive(Clone, Debug)]
    pub enum RemoteCodebaseSearchAvailability {
        NoConnectedHost,
        NoActiveRepo,
        NotIndexed {
            remote_path: RemotePath,
        },
        Indexing {
            remote_path: RemotePath,
        },
        Unavailable {
            remote_path: RemotePath,
            message: String,
        },
        Ready(RemoteCodebaseSearchContext),
    }

    #[derive(Clone, Debug)]
    pub struct RemoteCodebaseSearchContext {
        pub remote_path: RemotePath,
        pub is_stale: bool,
    }

    #[derive(Clone, Debug)]
    pub struct RemoteCodebaseContextEntry {
        pub name: String,
        pub path: String,
    }

    pub struct RemoteCodebaseIndexModel;

    impl RemoteCodebaseIndexModel {
        pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
            Self
        }

        pub fn entries_for_settings(&self) -> Vec<RemoteCodebaseIndexSettingsEntry> {
            Vec::new()
        }

        pub fn codebases_for_agent_context(&self) -> Vec<RemoteCodebaseContextEntry> {
            Vec::new()
        }

        pub fn active_repo_path(
            &self,
            _session_context: &crate::ai::blocklist::SessionContext,
            _requested_codebase_path: Option<&str>,
        ) -> Option<String> {
            None
        }

        pub fn active_repo_availability(
            &self,
            _session_context: &crate::ai::blocklist::SessionContext,
            _requested_codebase_path: Option<&str>,
        ) -> RemoteCodebaseSearchAvailability {
            RemoteCodebaseSearchAvailability::NoConnectedHost
        }

        pub fn request_index(&mut self, _remote_path: RemotePath, _ctx: &mut ModelContext<Self>) {}
        pub fn resync_index(&mut self, _remote_path: RemotePath, _ctx: &mut ModelContext<Self>) {}
        pub fn drop_index(&mut self, _remote_path: RemotePath, _ctx: &mut ModelContext<Self>) {}
    }

    impl Entity for RemoteCodebaseIndexModel {
        type Event = RemoteCodebaseIndexModelEvent;
    }

    impl SingletonEntity for RemoteCodebaseIndexModel {}
}
