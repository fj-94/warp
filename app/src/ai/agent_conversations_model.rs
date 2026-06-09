#[allow(dead_code)]
pub mod entry;

pub use entry::{
    AgentConversationEntry, AgentConversationEntryId, AgentConversationNavigationSubject,
    AgentConversationProvenance,
};

use crate::ai::active_agent_views_model::ActiveAgentViewsModel;
use crate::ai::agent::api::ServerConversationToken;
use crate::ai::agent::conversation::{AIConversationId, ConversationStatus};
use crate::ai::ambient_agents::AmbientAgentTaskId;
use crate::ai::ambient_agents::{
    AgentSource, AmbientAgentLiveSessionState, AmbientAgentTask, AmbientAgentTaskState,
};
use crate::ai::artifacts::Artifact;
use crate::ai::blocklist::{
    BlocklistAIHistoryEvent, BlocklistAIHistoryModel, ConversationStatusUpdate,
};
use crate::ai::cloud_environments::CloudAmbientAgentEnvironment;
use crate::ai::conversation_navigation::ConversationNavigationData;
use crate::auth::AuthStateProvider;
use crate::server::cloud_objects::update_manager::{UpdateManager, UpdateManagerEvent};
use crate::server::ids::{ServerId, SyncId};
use crate::ui_components::icons::Icon;
use crate::workspace::{RestoreConversationLayout, WorkspaceAction};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use futures::stream::AbortHandle;
use instant::Instant;
use itertools::Itertools;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use warp_cli::agent::Harness;
use warp_core::execution_mode::AppExecutionMode;
use warp_core::features::FeatureFlag;
use warp_core::ui::theme::{color::internal_colors, WarpTheme};
use warpui::color::ColorU;
use warpui::r#async::Timer;
use warpui::{AppContext, Entity, EntityId, ModelContext, SingletonEntity, WindowId};

const RTC_TASK_REFRESH_THROTTLE: Duration = Duration::from_secs(5);

/// How long to skip refetching a task that just failed with a transient error
/// (5xx / 408 / 429 / network). Short cooldown — `spawn_with_retry_on_error_when` already
/// runs fast exponential retries before bubbling up the failure, so this is just enough to
/// absorb streaming-driven re-entries from `update_transcript_details_panel_data`.
const TRANSIENT_FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(2);

/// How long to skip refetching a task that just failed with a permanent (non-transient) HTTP
/// error such as 401/403/404. We don't refuse forever — permissions can change mid-session
/// (e.g. an ACL grant) — but we wait long enough that streaming bursts and rapid re-entries
/// can't cause a flood.
const PERMANENT_FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

/// Per-task fetch state for `get_or_async_fetch_task_data`. The three variants are mutually
/// exclusive: a task id is either being fetched right now, in a short cooldown after a
/// transient failure, or in a longer cooldown after a permanent (non-transient) failure.
#[derive(Debug)]
#[allow(dead_code)]
enum TaskFetchState {
    /// A retry chain is currently outstanding for this task id. Used to dedupe re-entries
    /// (e.g. from streaming-driven panel refreshes) so we don't spawn overlapping retry
    /// chains for the same task id.
    InFlight,
    /// The fetch returned a permanent (non-transient) HTTP error such as 401/403/404; remember
    /// when it failed so we can back off for [`PERMANENT_FETCH_FAILURE_COOLDOWN`] before
    /// retrying. We don't refuse forever in case permissions change mid-session.
    /// The `String` carries a human-readable description of the failure for display in the UI.
    PermanentlyFailed { at: Instant, message: String },
    /// The retry chain just exhausted on a transient error; remember when it failed so we
    /// can back off for [`TRANSIENT_FETCH_FAILURE_COOLDOWN`] before retrying.
    /// The `String` carries a human-readable description of the failure for display in the UI.
    TransientlyFailed { at: Instant, message: String },
}

/// Tracks the cooldown window for RTC-triggered task-list refreshes. Pending events keep
/// the earliest timestamp in the burst because `updated_after` is a lower bound; using the
/// latest timestamp could skip tasks that changed earlier in the same window.
#[derive(Default)]
enum RtcTaskRefreshThrottleState {
    #[default]
    Idle,
    CoolingDown {
        pending_timestamp: Option<DateTime<Utc>>,
        timer_abort_handle: AbortHandle,
    },
}

fn record_earliest_rtc_task_refresh_timestamp(
    pending_timestamp: &mut Option<DateTime<Utc>>,
    timestamp: DateTime<Utc>,
) {
    match pending_timestamp {
        Some(existing_timestamp) if timestamp < *existing_timestamp => {
            *existing_timestamp = timestamp;
        }
        None => {
            *pending_timestamp = Some(timestamp);
        }
        Some(_) => {}
    }
}

/// Protected eviction: we'll always keep at least 200 personal tasks in the model.
/// This is so that whenever we evict stale tasks, we do not evict relevant, recent personal tasks
/// (e.g. if I load in 500 team Slack tasks from today, we should _not_ evict my personal conversation
/// from yesterday).
#[cfg(test)]
const MAX_PERSONAL_TASKS: usize = 200;
#[cfg(test)]
const MAX_TEAM_TASKS: usize = 300;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Available,
    Expired,
    Unavailable,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum StatusFilter {
    #[default]
    All,
    Working,
    Done,
    Failed,
}

impl StatusFilter {
    /// Returns `true` if a status transition from `prev_bucket` to `new_bucket` flips
    /// whether an item is included by this filter. `All` matches every bucket so it
    /// is never crossed; the other variants are crossed when exactly one of the buckets
    /// equals this filter.
    pub(crate) fn is_membership_crossed(
        self,
        prev_bucket: StatusFilter,
        new_bucket: StatusFilter,
    ) -> bool {
        match self {
            StatusFilter::All => false,
            StatusFilter::Working | StatusFilter::Done | StatusFilter::Failed => {
                (prev_bucket == self) != (new_bucket == self)
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum SourceFilter {
    #[default]
    All,
    Specific(AgentSource),
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum CreatorFilter {
    #[default]
    All,
    Specific {
        name: String,
        uid: String,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ArtifactFilter {
    #[default]
    All,
    PullRequest,
    Plan,
    Screenshot,
    File,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum CreatedOnFilter {
    #[default]
    All,
    Last24Hours,
    Past3Days,
    LastWeek,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum EnvironmentFilter {
    #[default]
    All,
    NoEnvironment,
    Specific(String),
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerFilter {
    All,
    #[default]
    PersonalOnly,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum HarnessFilter {
    #[default]
    All,
    Specific(Harness),
}

impl Serialize for HarnessFilter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            HarnessFilter::All => serializer.serialize_str("all"),
            HarnessFilter::Specific(harness) => serializer.collect_str(harness),
        }
    }
}

impl<'de> Deserialize<'de> for HarnessFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Harness::from_str(&raw, false)
            .ok()
            .map(HarnessFilter::Specific)
            .unwrap_or(HarnessFilter::All))
    }
}

#[derive(Default, PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
pub struct AgentManagementFilters {
    pub owners: OwnerFilter,
    pub status: StatusFilter,
    pub source: SourceFilter,
    pub created_on: CreatedOnFilter,
    pub creator: CreatorFilter,
    pub artifact: ArtifactFilter,
    #[serde(default)]
    pub environment: EnvironmentFilter,
    #[serde(default)]
    pub harness: HarnessFilter,
}

impl AgentManagementFilters {
    pub fn reset_all_but_owner(&mut self) {
        self.status = StatusFilter::default();
        self.source = SourceFilter::default();
        self.created_on = CreatedOnFilter::default();
        self.creator = CreatorFilter::default();
        self.artifact = ArtifactFilter::default();
        self.environment = EnvironmentFilter::default();
        self.harness = HarnessFilter::default();
    }

    pub fn is_filtering(&self) -> bool {
        self.status != StatusFilter::default()
            || self.source != SourceFilter::default()
            || self.created_on != CreatedOnFilter::default()
            || self.creator != CreatorFilter::default() && self.owners != OwnerFilter::PersonalOnly
            || self.artifact != ArtifactFilter::default()
            || self.environment != EnvironmentFilter::default()
            || self.harness != HarnessFilter::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentRunDisplayStatus {
    /// Raw task-service lifecycle states. `from_task` only returns `TaskInProgress` while the
    /// task still has an active execution, or when there is no shadowed local conversation to
    /// provide a more granular status.
    TaskQueued,
    TaskPending,
    TaskClaimed,
    TaskInProgress,
    TaskSucceeded,
    TaskFailed,
    TaskError,
    TaskBlocked {
        blocked_action: String,
    },
    TaskCancelled,
    TaskUnknown,
    /// Conversation-derived lifecycle states, used for interactive conversations and for
    /// in-progress ambient tasks after they can be resolved to their shadowed local conversation.
    ConversationInProgress,
    ConversationSucceeded,
    ConversationError,
    ConversationBlocked {
        blocked_action: String,
    },
    ConversationCancelled,
}

impl AgentRunDisplayStatus {
    pub fn from_task(task: &AmbientAgentTask, app: &AppContext) -> Self {
        match &task.state {
            AmbientAgentTaskState::Queued
            | AmbientAgentTaskState::Pending
            | AmbientAgentTaskState::Claimed => Self::from_task_state(task),
            AmbientAgentTaskState::InProgress => {
                if task.has_active_execution() {
                    return Self::from_task_state(task);
                }
                let history_model = BlocklistAIHistoryModel::as_ref(app);
                entry::conversation_id_shadowed_by_task(task, history_model)
                    .and_then(|conversation_id| history_model.conversation(&conversation_id))
                    .map(|conversation| Self::from_conversation_status(conversation.status()))
                    .unwrap_or_else(|| Self::from_task_state(task))
            }
            AmbientAgentTaskState::Succeeded
            | AmbientAgentTaskState::Failed
            | AmbientAgentTaskState::Error
            | AmbientAgentTaskState::Blocked
            | AmbientAgentTaskState::Cancelled
            | AmbientAgentTaskState::Unknown => Self::from_task_state(task),
        }
    }

    pub fn from_conversation_status(status: &ConversationStatus) -> Self {
        match status {
            ConversationStatus::InProgress => Self::ConversationInProgress,
            ConversationStatus::Success => Self::ConversationSucceeded,
            ConversationStatus::Error => Self::ConversationError,
            ConversationStatus::Cancelled => Self::ConversationCancelled,
            ConversationStatus::Blocked { blocked_action } => Self::ConversationBlocked {
                blocked_action: blocked_action.clone(),
            },
        }
    }

    fn from_task_state(task: &AmbientAgentTask) -> Self {
        match &task.state {
            AmbientAgentTaskState::Queued => Self::TaskQueued,
            AmbientAgentTaskState::Pending => Self::TaskPending,
            AmbientAgentTaskState::Claimed => Self::TaskClaimed,
            AmbientAgentTaskState::InProgress => Self::TaskInProgress,
            AmbientAgentTaskState::Succeeded => Self::TaskSucceeded,
            AmbientAgentTaskState::Failed => Self::TaskFailed,
            AmbientAgentTaskState::Error => Self::TaskError,
            AmbientAgentTaskState::Blocked => Self::TaskBlocked {
                blocked_action: task
                    .status_message
                    .as_ref()
                    .map(|m| m.message.clone())
                    .unwrap_or_else(|| "Task blocked".to_string()),
            },
            AmbientAgentTaskState::Cancelled => Self::TaskCancelled,
            AmbientAgentTaskState::Unknown => Self::TaskUnknown,
        }
    }

    pub fn status_filter(&self) -> StatusFilter {
        match self {
            AgentRunDisplayStatus::TaskQueued
            | AgentRunDisplayStatus::TaskPending
            | AgentRunDisplayStatus::TaskClaimed
            | AgentRunDisplayStatus::TaskInProgress
            | AgentRunDisplayStatus::ConversationInProgress => StatusFilter::Working,
            AgentRunDisplayStatus::TaskSucceeded | AgentRunDisplayStatus::ConversationSucceeded => {
                StatusFilter::Done
            }
            AgentRunDisplayStatus::TaskFailed
            | AgentRunDisplayStatus::TaskError
            | AgentRunDisplayStatus::TaskBlocked { .. }
            | AgentRunDisplayStatus::TaskCancelled
            | AgentRunDisplayStatus::TaskUnknown
            | AgentRunDisplayStatus::ConversationError
            | AgentRunDisplayStatus::ConversationBlocked { .. }
            | AgentRunDisplayStatus::ConversationCancelled => StatusFilter::Failed,
        }
    }

    pub fn to_conversation_status(&self) -> ConversationStatus {
        match self {
            AgentRunDisplayStatus::TaskQueued
            | AgentRunDisplayStatus::TaskPending
            | AgentRunDisplayStatus::TaskClaimed
            | AgentRunDisplayStatus::TaskInProgress
            | AgentRunDisplayStatus::ConversationInProgress => ConversationStatus::InProgress,
            AgentRunDisplayStatus::TaskSucceeded | AgentRunDisplayStatus::ConversationSucceeded => {
                ConversationStatus::Success
            }
            AgentRunDisplayStatus::TaskFailed
            | AgentRunDisplayStatus::TaskError
            | AgentRunDisplayStatus::TaskUnknown
            | AgentRunDisplayStatus::ConversationError => ConversationStatus::Error,
            AgentRunDisplayStatus::TaskBlocked { blocked_action }
            | AgentRunDisplayStatus::ConversationBlocked { blocked_action } => {
                ConversationStatus::Blocked {
                    blocked_action: blocked_action.clone(),
                }
            }
            AgentRunDisplayStatus::TaskCancelled | AgentRunDisplayStatus::ConversationCancelled => {
                ConversationStatus::Cancelled
            }
        }
    }

    pub fn is_cancellable(&self) -> bool {
        self.is_working()
    }

    pub fn is_working(&self) -> bool {
        matches!(
            self,
            AgentRunDisplayStatus::TaskQueued
                | AgentRunDisplayStatus::TaskPending
                | AgentRunDisplayStatus::TaskClaimed
                | AgentRunDisplayStatus::TaskInProgress
                | AgentRunDisplayStatus::ConversationInProgress
        )
    }

    pub fn status_icon_and_color(&self, theme: &WarpTheme) -> (Icon, ColorU) {
        match self {
            AgentRunDisplayStatus::TaskQueued
            | AgentRunDisplayStatus::TaskPending
            | AgentRunDisplayStatus::TaskClaimed
            | AgentRunDisplayStatus::TaskInProgress
            | AgentRunDisplayStatus::ConversationInProgress => {
                (Icon::ClockLoader, theme.ansi_fg_magenta())
            }
            AgentRunDisplayStatus::TaskSucceeded | AgentRunDisplayStatus::ConversationSucceeded => {
                (Icon::Check, theme.ansi_fg_green())
            }
            AgentRunDisplayStatus::TaskFailed
            | AgentRunDisplayStatus::TaskError
            | AgentRunDisplayStatus::TaskUnknown
            | AgentRunDisplayStatus::ConversationError => (Icon::Triangle, theme.ansi_fg_red()),
            AgentRunDisplayStatus::TaskBlocked { .. }
            | AgentRunDisplayStatus::ConversationBlocked { .. } => {
                (Icon::StopFilled, theme.ansi_fg_yellow())
            }
            AgentRunDisplayStatus::TaskCancelled => (
                Icon::Cancelled,
                theme.disabled_text_color(theme.background()).into_solid(),
            ),
            AgentRunDisplayStatus::ConversationCancelled => {
                (Icon::StopFilled, internal_colors::neutral_5(theme))
            }
        }
    }
}

impl std::fmt::Display for AgentRunDisplayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentRunDisplayStatus::TaskQueued => write!(f, "Queued"),
            AgentRunDisplayStatus::TaskPending => write!(f, "Pending"),
            AgentRunDisplayStatus::TaskClaimed => write!(f, "Claimed"),
            AgentRunDisplayStatus::TaskInProgress
            | AgentRunDisplayStatus::ConversationInProgress => write!(f, "In progress"),
            AgentRunDisplayStatus::TaskSucceeded | AgentRunDisplayStatus::ConversationSucceeded => {
                write!(f, "Done")
            }
            AgentRunDisplayStatus::TaskFailed => write!(f, "Failed"),
            AgentRunDisplayStatus::TaskError | AgentRunDisplayStatus::ConversationError => {
                write!(f, "Error")
            }
            AgentRunDisplayStatus::TaskBlocked { .. }
            | AgentRunDisplayStatus::ConversationBlocked { .. } => write!(f, "Blocked"),
            AgentRunDisplayStatus::TaskCancelled | AgentRunDisplayStatus::ConversationCancelled => {
                write!(f, "Cancelled")
            }
            AgentRunDisplayStatus::TaskUnknown => write!(f, "Failed"),
        }
    }
}

/// Stores conversation metadata needed for display in conversation/task views.
pub struct ConversationMetadata {
    pub nav_data: ConversationNavigationData,
}

pub(crate) fn artifacts_match_filter(
    artifacts: &[Artifact],
    artifact_filter: &ArtifactFilter,
) -> bool {
    match artifact_filter {
        ArtifactFilter::All => true,
        ArtifactFilter::PullRequest => artifacts
            .iter()
            .any(|artifact| matches!(artifact, Artifact::PullRequest { .. })),
        ArtifactFilter::Plan => artifacts
            .iter()
            .any(|artifact| matches!(artifact, Artifact::Plan { .. })),
        ArtifactFilter::Screenshot => artifacts
            .iter()
            .any(|artifact| matches!(artifact, Artifact::Screenshot { .. })),
        ArtifactFilter::File => artifacts
            .iter()
            .any(|artifact| matches!(artifact, Artifact::File { .. })),
    }
}

/// This model serves as a unified interface for reading both local and ambient agent conversations
/// (i.e. conversations & tasks). The model is responsible for polling for new tasks and updating
/// its local state accordingly.
///
/// This model backs both the agent management view and the conversation list view.
pub struct AgentConversationsModel {
    /// A map of task IDs to agent tasks.
    tasks: HashMap<AmbientAgentTaskId, AmbientAgentTask>,
    /// A map of conversation IDs to local conversations.
    conversations: HashMap<AIConversationId, ConversationMetadata>,
    /// Handle to abort the in-flight polling request.
    in_flight_poll_abort_handle: Option<AbortHandle>,
    /// Handle to abort the timer for initiating the next poll.
    next_poll_abort_handle: Option<AbortHandle>,
    /// Set of view IDs actively consuming this model's data per window.
    /// When a window has at least one consumer, we poll for new tasks while that window is active.
    active_data_consumers_per_window: HashMap<WindowId, HashSet<EntityId>>,
    /// Whether we have finished the initial task load
    has_finished_initial_load: bool,
    /// Per-task fetch state for `get_or_async_fetch_task_data`. See [`TaskFetchState`] for
    /// the meaning of each variant. Tasks that have been successfully fetched live in `tasks`
    /// and are absent from this map.
    task_fetch_state: HashMap<AmbientAgentTaskId, TaskFetchState>,
    rtc_task_refresh_throttle_state: RtcTaskRefreshThrottleState,
}

pub enum AgentConversationsModelEvent {
    /// Initial load of tasks completed.
    ConversationsLoaded,
    /// New tasks were received during polling (view should diff against its local state).
    #[allow(dead_code)]
    NewTasksReceived,
    /// Existing task data may have been updated (e.g., state changes).
    TasksUpdated,
    /// Conversation status data was updated
    ConversationUpdated { kind: ConversationUpdateKind },
    /// Conversation artifacts were updated (plans, PRs, etc.)
    ConversationArtifactsUpdated { conversation_id: AIConversationId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationUpdateKind {
    /// The conversation was re-loaded into a terminal view.
    Restored,
    /// The conversation's status was set.
    StatusSet {
        prev_filter: StatusFilter,
        new_filter: StatusFilter,
    },
    /// Conversation metadata or capabilities changed.
    MetadataChanged,
}

impl Entity for AgentConversationsModel {
    type Event = AgentConversationsModelEvent;
}

impl SingletonEntity for AgentConversationsModel {}

impl AgentConversationsModel {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        // If FF not enabled, return an empty model and don't sync any tasks.
        if !FeatureFlag::AgentManagementView.is_enabled() {
            return Self {
                tasks: HashMap::new(),
                conversations: HashMap::new(),
                in_flight_poll_abort_handle: None,
                next_poll_abort_handle: None,
                active_data_consumers_per_window: HashMap::new(),
                has_finished_initial_load: true,
                task_fetch_state: HashMap::new(),
                rtc_task_refresh_throttle_state: RtcTaskRefreshThrottleState::default(),
            };
        }

        let history_model = BlocklistAIHistoryModel::handle(ctx);
        ctx.subscribe_to_model(&history_model, move |me, event, ctx| {
            me.handle_history_event(event, ctx);
        });

        let active_views_model = ActiveAgentViewsModel::handle(ctx);
        ctx.subscribe_to_model(&active_views_model, |me, _event, ctx| {
            me.sync_conversations(ctx);
        });

        // Subscribe to UpdateManager for RTC task updates
        if FeatureFlag::AmbientAgentsRTC.is_enabled() {
            let update_manager = UpdateManager::handle(ctx);
            ctx.subscribe_to_model(&update_manager, Self::handle_update_manager_event);
        }

        let mut model = Self {
            tasks: HashMap::new(),
            conversations: HashMap::new(),
            in_flight_poll_abort_handle: None,
            next_poll_abort_handle: None,
            active_data_consumers_per_window: HashMap::new(),
            has_finished_initial_load: false,
            task_fetch_state: HashMap::new(),
            rtc_task_refresh_throttle_state: RtcTaskRefreshThrottleState::default(),
        };

        // Only sync local conversations if we're not in CLI mode. Local conversation storage is
        // authoritative in the OSS local-agent build, so startup does not wait on auth or cloud
        // task metadata.
        if AppExecutionMode::as_ref(ctx).can_fetch_agent_runs_for_management() {
            model.sync_conversations(ctx);
        }
        model.has_finished_initial_load = true;
        model
    }

    pub fn is_loading(&self) -> bool {
        !self.has_finished_initial_load
    }

    fn handle_update_manager_event(
        &mut self,
        event: &UpdateManagerEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        if let UpdateManagerEvent::AmbientTaskUpdated { timestamp } = event {
            match std::mem::take(&mut self.rtc_task_refresh_throttle_state) {
                RtcTaskRefreshThrottleState::Idle => {
                    self.fetch_tasks_updated_after(*timestamp, ctx);
                    self.start_rtc_task_refresh_throttle_timer(ctx);
                }
                RtcTaskRefreshThrottleState::CoolingDown {
                    mut pending_timestamp,
                    timer_abort_handle,
                } => {
                    record_earliest_rtc_task_refresh_timestamp(&mut pending_timestamp, *timestamp);
                    self.rtc_task_refresh_throttle_state =
                        RtcTaskRefreshThrottleState::CoolingDown {
                            pending_timestamp,
                            timer_abort_handle,
                        };
                }
            }
        }
    }

    fn start_rtc_task_refresh_throttle_timer(&mut self, ctx: &mut ModelContext<Self>) {
        let future_handle = ctx.spawn(
            async move {
                Timer::after(RTC_TASK_REFRESH_THROTTLE).await;
            },
            |model, _, ctx| {
                let pending_timestamp =
                    match std::mem::take(&mut model.rtc_task_refresh_throttle_state) {
                        RtcTaskRefreshThrottleState::Idle => None,
                        RtcTaskRefreshThrottleState::CoolingDown {
                            pending_timestamp, ..
                        } => pending_timestamp,
                    };

                if let Some(timestamp) = pending_timestamp {
                    model.fetch_tasks_updated_after(timestamp, ctx);
                    model.start_rtc_task_refresh_throttle_timer(ctx);
                }
            },
        );
        self.rtc_task_refresh_throttle_state = RtcTaskRefreshThrottleState::CoolingDown {
            pending_timestamp: None,
            timer_abort_handle: future_handle.abort_handle(),
        };
    }

    fn abort_rtc_task_refresh_throttle(&mut self) {
        if let RtcTaskRefreshThrottleState::CoolingDown {
            timer_abort_handle, ..
        } = std::mem::take(&mut self.rtc_task_refresh_throttle_state)
        {
            timer_abort_handle.abort();
        }
    }

    /// Refresh local conversations when a remote task update signal arrives.
    fn fetch_tasks_updated_after(
        &mut self,
        _timestamp: DateTime<Utc>,
        ctx: &mut ModelContext<Self>,
    ) {
        self.sync_conversations(ctx);
    }

    /// Sync all conversations to the AgentConversationsModel.
    ///
    /// This function will loop through all active panes, recently closed panes, and historical
    /// conversations to construct a complete snapshot of conversations.
    pub fn sync_conversations(&mut self, ctx: &mut ModelContext<Self>) {
        if !FeatureFlag::InteractiveConversationManagementView.is_enabled() {
            return;
        }

        let nav_data_list = ConversationNavigationData::all_conversations(ctx);

        self.conversations.clear();
        for nav_data in nav_data_list {
            let conversation_id = nav_data.id;
            let metadata = ConversationMetadata { nav_data };
            self.conversations.insert(conversation_id, metadata);
        }

        ctx.emit(AgentConversationsModelEvent::ConversationsLoaded);
    }

    /// Called when a view that consumes this model's data becomes visible.
    /// Uses view_id to make registration idempotent.
    pub fn register_view_open(
        &mut self,
        window_id: WindowId,
        view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        self.active_data_consumers_per_window
            .entry(window_id)
            .or_default()
            .insert(view_id);
        self.update_polling_state(ctx);
    }

    /// Called when a view that consumes this model's data becomes hidden.
    /// Uses view_id to make unregistration idempotent.
    pub fn register_view_closed(
        &mut self,
        window_id: WindowId,
        view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        if let Some(views) = self.active_data_consumers_per_window.get_mut(&window_id) {
            views.remove(&view_id);
            if views.is_empty() {
                self.active_data_consumers_per_window.remove(&window_id);
            }
        }
        self.update_polling_state(ctx);
    }

    /// Updates the polling state based on whether the active window has the view open.
    fn update_polling_state(&mut self, ctx: &mut ModelContext<Self>) {
        let should_poll = self.should_be_polling(ctx);

        if should_poll && self.next_poll_abort_handle.is_none() {
            self.poll_for_tasks(ctx);
        } else if !should_poll {
            self.abort_existing_poll();
        }
    }

    /// Returns true if we should be polling: online, not loading, and active window has the view open.
    fn should_be_polling(&self, _ctx: &ModelContext<Self>) -> bool {
        false
    }

    /// Abort the current in-flight poll (does NOT abort initial sync)
    fn abort_existing_poll(&mut self) {
        if let Some(handle) = self.next_poll_abort_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.in_flight_poll_abort_handle.take() {
            handle.abort();
        }
    }

    fn poll_for_tasks(&mut self, ctx: &mut ModelContext<Self>) {
        let _ = ctx;
        self.abort_existing_poll();
    }

    /// Returns true if we have tasks or local conversations in this view
    pub fn has_items(&self) -> bool {
        !self.tasks.is_empty() || !self.conversations.is_empty()
    }

    /// Returns an iterator over all ambient agent tasks.
    pub fn tasks_iter(&self) -> impl Iterator<Item = &AmbientAgentTask> {
        self.tasks.values()
    }

    #[cfg(test)]
    pub(crate) fn insert_task_for_test(&mut self, task: AmbientAgentTask) {
        self.tasks.insert(task.task_id, task);
    }

    pub(crate) fn mark_task_execution_ended(
        &mut self,
        task_id: AmbientAgentTaskId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(task) = self.tasks.get_mut(&task_id) else {
            return;
        };
        let was_active = task.has_active_execution();
        task.is_sandbox_running = false;
        if was_active {
            ctx.emit(AgentConversationsModelEvent::TasksUpdated);
        }
    }

    /// Returns normalized, owned entries for agent management/navigation surfaces.
    pub fn get_entries(
        &self,
        filters: &AgentManagementFilters,
        app: &AppContext,
    ) -> Vec<AgentConversationEntry> {
        let history_model = BlocklistAIHistoryModel::as_ref(app);
        let mut entries = Vec::new();
        let mut attached_conversation_ids = HashSet::new();
        let mut emitted_conversation_ids = HashSet::new();

        for task in self.tasks.values() {
            let entry = entry::entry_for_task(task, history_model, app);
            if let Some(conversation_id) = entry.identity.local_conversation_id {
                attached_conversation_ids.insert(conversation_id);
            }
            entries.push(entry);
        }

        for metadata in self.conversations.values() {
            let conversation_id = metadata.nav_data.id;
            if attached_conversation_ids.contains(&conversation_id) {
                continue;
            }
            let entry = entry::entry_for_conversation(metadata, history_model, app);
            emitted_conversation_ids.insert(conversation_id);
            entries.push(entry);
        }

        for metadata in history_model.get_local_conversations_metadata() {
            if !metadata.has_local_data {
                continue;
            }
            if attached_conversation_ids.contains(&metadata.id)
                || emitted_conversation_ids.contains(&metadata.id)
            {
                continue;
            }
            let nav_data =
                ConversationNavigationData::from_historical_conversation_metadata(metadata);
            entries.push(entry::entry_for_historical_metadata(
                metadata,
                nav_data,
                history_model,
                app,
            ));
        }

        entries
            .into_iter()
            .filter(|entry| entry.matches_filters(filters, app))
            .sorted_by(|a, b| b.display.last_updated.cmp(&a.display.last_updated))
            .collect()
    }

    pub fn get_entry_by_id(
        &self,
        id: &AgentConversationEntryId,
        app: &AppContext,
    ) -> Option<AgentConversationEntry> {
        let history_model = BlocklistAIHistoryModel::as_ref(app);
        match id {
            AgentConversationEntryId::AmbientRun(task_id) => self
                .tasks
                .get(task_id)
                .map(|task| entry::entry_for_task(task, history_model, app)),
            AgentConversationEntryId::Conversation(conversation_id) => self
                .conversations
                .get(conversation_id)
                .map(|metadata| entry::entry_for_conversation(metadata, history_model, app))
                .or_else(|| {
                    history_model
                        .get_conversation_metadata(conversation_id)
                        .filter(|metadata| metadata.has_local_data)
                        .map(|metadata| {
                            let nav_data =
                                ConversationNavigationData::from_historical_conversation_metadata(
                                    metadata,
                                );
                            entry::entry_for_historical_metadata(
                                metadata,
                                nav_data,
                                history_model,
                                app,
                            )
                        })
                }),
        }
    }

    pub fn resolve_open_action(
        subject: AgentConversationNavigationSubject,
        restore_layout: Option<RestoreConversationLayout>,
        app: &AppContext,
    ) -> Option<WorkspaceAction> {
        let model = Self::as_ref(app);
        match subject {
            AgentConversationNavigationSubject::Entry(id) => model
                .get_entry_by_id(&id, app)
                .and_then(|entry| model.resolve_entry_open_action(&entry, restore_layout, app)),
            AgentConversationNavigationSubject::ServerToken(server_token) => model
                .entry_for_server_token(&server_token, app)
                .and_then(|entry| model.resolve_entry_open_action(&entry, restore_layout, app)),
        }
    }

    pub fn resolve_copy_link(
        subject: AgentConversationNavigationSubject,
        app: &AppContext,
    ) -> Option<String> {
        let model = Self::as_ref(app);
        match subject {
            AgentConversationNavigationSubject::Entry(id) => model
                .get_entry_by_id(&id, app)
                .and_then(|entry| model.resolve_entry_copy_link(&entry)),
            AgentConversationNavigationSubject::ServerToken(server_token) => model
                .entry_for_server_token(&server_token, app)
                .and_then(|entry| model.resolve_entry_copy_link(&entry)),
        }
    }

    fn resolve_entry_open_action(
        &self,
        entry: &AgentConversationEntry,
        restore_layout: Option<RestoreConversationLayout>,
        app: &AppContext,
    ) -> Option<WorkspaceAction> {
        let active_views_model = ActiveAgentViewsModel::as_ref(app);

        if let Some(task_id) = entry.identity.ambient_agent_task_id {
            match self
                .tasks
                .get(&task_id)
                .map(AmbientAgentTask::active_live_session_state)
            {
                Some(AmbientAgentLiveSessionState::Attachable { session_id }) => {
                    return Some(WorkspaceAction::OpenOrAttachAmbientAgentConversation {
                        session_id,
                        task_id,
                    });
                }
                Some(AmbientAgentLiveSessionState::ActiveUnattachable) => {
                    return active_views_model
                        .get_terminal_view_id_for_ambient_task(task_id)
                        .map(
                            |terminal_view_id| WorkspaceAction::FocusTerminalViewInWorkspace {
                                terminal_view_id,
                            },
                        );
                }
                Some(AmbientAgentLiveSessionState::Inactive) | None => {}
            }

            if let Some(terminal_view_id) =
                active_views_model.get_terminal_view_id_for_ambient_task(task_id)
            {
                return Some(WorkspaceAction::FocusTerminalViewInWorkspace { terminal_view_id });
            }
        }

        if let Some(conversation_id) = entry.identity.local_conversation_id {
            if active_views_model.is_conversation_open(conversation_id, app) {
                if let Some(nav_data) = self
                    .conversations
                    .get(&conversation_id)
                    .map(|metadata| &metadata.nav_data)
                {
                    return Some(WorkspaceAction::RestoreOrNavigateToConversation {
                        conversation_id,
                        window_id: nav_data.window_id,
                        pane_view_locator: nav_data.pane_view_locator,
                        terminal_view_id: nav_data.terminal_view_id,
                        restore_layout,
                    });
                }

                if let Some(terminal_view_id) =
                    active_views_model.get_terminal_view_id_for_conversation(conversation_id, app)
                {
                    return Some(WorkspaceAction::FocusTerminalViewInWorkspace {
                        terminal_view_id,
                    });
                }
            }
        }

        if let Some(conversation_id) = entry.identity.local_conversation_id {
            let nav_data = self
                .conversations
                .get(&conversation_id)
                .map(|metadata| &metadata.nav_data);
            if !entry.backing.has_cloud_data
                || entry.backing.has_local_persisted_data
                || entry.backing.has_loaded_conversation
                || nav_data.is_some()
            {
                return Some(WorkspaceAction::RestoreOrNavigateToConversation {
                    conversation_id,
                    window_id: nav_data.and_then(|nav_data| nav_data.window_id),
                    pane_view_locator: None,
                    terminal_view_id: nav_data.and_then(|nav_data| nav_data.terminal_view_id),
                    restore_layout,
                });
            }
        }

        None
    }

    fn resolve_entry_copy_link(&self, _entry: &AgentConversationEntry) -> Option<String> {
        None
    }

    fn entry_for_server_token(
        &self,
        server_token: &ServerConversationToken,
        app: &AppContext,
    ) -> Option<AgentConversationEntry> {
        let history_model = BlocklistAIHistoryModel::as_ref(app);
        if let Some(task) = self.tasks.values().find(|task| {
            task.conversation_id()
                .is_some_and(|conversation_id| conversation_id == server_token.as_str())
        }) {
            return Some(entry::entry_for_task(task, history_model, app));
        }

        let conversation_id = history_model.find_conversation_id_by_server_token(server_token)?;
        if let Some(task) = self.tasks.values().find(|task| {
            entry::conversation_id_shadowed_by_task(task, history_model) == Some(conversation_id)
        }) {
            return Some(entry::entry_for_task(task, history_model, app));
        }

        self.get_entry_by_id(
            &AgentConversationEntryId::Conversation(conversation_id),
            app,
        )
    }

    fn handle_history_event(
        &mut self,
        event: &BlocklistAIHistoryEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        if !FeatureFlag::InteractiveConversationManagementView.is_enabled() {
            return;
        }
        match event {
            // Events that affect conversation navigation data - need full sync
            BlocklistAIHistoryEvent::StartedNewConversation { .. }
            | BlocklistAIHistoryEvent::SetActiveConversation { .. }
            | BlocklistAIHistoryEvent::AppendedExchange { .. }
            | BlocklistAIHistoryEvent::SplitConversation { .. }
            | BlocklistAIHistoryEvent::RestoredConversations { .. }
            | BlocklistAIHistoryEvent::RemoveConversation { .. }
            | BlocklistAIHistoryEvent::DeletedConversation { .. }
            | BlocklistAIHistoryEvent::ClearedConversationsInTerminalView { .. }
            | BlocklistAIHistoryEvent::ClearedActiveConversation { .. } => {
                self.sync_conversations(ctx);
            }

            // Status changes - just trigger re-render since status is looked up at render time
            BlocklistAIHistoryEvent::UpdatedConversationStatus {
                update, new_status, ..
            } => {
                let kind = match update {
                    ConversationStatusUpdate::Restored => ConversationUpdateKind::Restored,
                    ConversationStatusUpdate::Changed { prev_status } => {
                        ConversationUpdateKind::StatusSet {
                            prev_filter: AgentRunDisplayStatus::from_conversation_status(
                                prev_status,
                            )
                            .status_filter(),
                            new_filter: AgentRunDisplayStatus::from_conversation_status(new_status)
                                .status_filter(),
                        }
                    }
                };
                ctx.emit(AgentConversationsModelEvent::ConversationUpdated { kind });
            }

            // Artifact changes - sync live artifacts into the cached task and notify.
            BlocklistAIHistoryEvent::UpdatedConversationArtifacts {
                conversation_id, ..
            } => {
                let conversation = BlocklistAIHistoryModel::as_ref(ctx).conversation(conversation_id);
                let Some(conversation) = conversation else {
                    return;
                };

                let task_id = conversation
                    .server_metadata()
                    .and_then(|metadata| metadata.ambient_agent_task_id);
                if let Some(task_id) = task_id {
                    // If the conversation is associated with a task, update the saved task
                    // with live artifacts.
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        task.artifacts = conversation.artifacts().to_vec();
                        ctx.emit(AgentConversationsModelEvent::TasksUpdated);
                    }
                }
                ctx.emit(AgentConversationsModelEvent::ConversationArtifactsUpdated {
                    conversation_id: *conversation_id,
                });
            }

            // Task/exchange-level changes that don't affect conversation navigation.
            BlocklistAIHistoryEvent::CreatedSubtask { .. }
            | BlocklistAIHistoryEvent::UpgradedTask { .. }
            | BlocklistAIHistoryEvent::ReassignedExchange { .. }
            | BlocklistAIHistoryEvent::UpdatedTodoList { .. }
            | BlocklistAIHistoryEvent::UpdatedAutoexecuteOverride { .. }
            | BlocklistAIHistoryEvent::UpdatedConversationMetadata { .. }
            // UpdatedStreamingExchange covers streaming and other exchange-level updates but
            // doesn't change any ConversationNavigationData fields (title comes from
            // UpdateTaskDescription, last_updated uses exchange.start_time which is set at append time).
            | BlocklistAIHistoryEvent::UpdatedStreamingExchange { .. }
            | BlocklistAIHistoryEvent::ConversationOwnershipTransferred { .. }
            | BlocklistAIHistoryEvent::NewConversationRequestComplete { .. }
            | BlocklistAIHistoryEvent::OrchestrationConfigUpdated { .. }
            | BlocklistAIHistoryEvent::ConversationUsageMetadataUpdated { .. } => {}

            BlocklistAIHistoryEvent::ConversationServerTokenAssigned { .. } => {
                ctx.emit(AgentConversationsModelEvent::ConversationUpdated {
                    kind: ConversationUpdateKind::MetadataChanged,
                });
            }
        }
    }

    /// Get raw task data by task ID
    pub fn get_task_data(&self, task_id: &AmbientAgentTaskId) -> Option<AmbientAgentTask> {
        self.tasks.get(task_id).cloned()
    }

    /// Returns the error message when the most recent fetch for `task_id` ended in a
    /// permanent or transient failure and the cooldown has not yet elapsed. The caller
    /// can use this to display an error state in the details panel.
    pub fn task_fetch_error(&self, task_id: &AmbientAgentTaskId) -> Option<&str> {
        match self.task_fetch_state.get(task_id) {
            Some(
                TaskFetchState::PermanentlyFailed { message, .. }
                | TaskFetchState::TransientlyFailed { message, .. },
            ) => Some(message),
            _ => None,
        }
    }

    /// Get raw task data by task ID from memory.
    /// If the task is already in memory, returns it immediately.
    /// In local conversation storage mode, missing task data is not fetched from Warp services;
    /// callers get a cached local-mode error instead.
    pub fn get_or_async_fetch_task_data(
        &mut self,
        task_id: &AmbientAgentTaskId,
        ctx: &mut ModelContext<Self>,
    ) -> Option<AmbientAgentTask> {
        // If we already have it, return it
        if let Some(task) = self.tasks.get(task_id) {
            return Some(task.clone());
        }

        // Consult the per-task fetch state. The three variants are mutually exclusive: at most
        // one applies to a given id.
        match self.task_fetch_state.get(task_id) {
            Some(TaskFetchState::InFlight) => return None,
            Some(TaskFetchState::PermanentlyFailed { at, .. }) => {
                if at.elapsed() < PERMANENT_FETCH_FAILURE_COOLDOWN {
                    return None;
                }
                // Cooldown has elapsed; clear the entry and fall through to fetch again.
                self.task_fetch_state.remove(task_id);
            }
            Some(TaskFetchState::TransientlyFailed { at, .. }) => {
                if at.elapsed() < TRANSIENT_FETCH_FAILURE_COOLDOWN {
                    return None;
                }
                self.task_fetch_state.remove(task_id);
            }
            None => {}
        }

        // Opportunistically purge other expired entries so the map doesn't grow unbounded.
        self.task_fetch_state.retain(|_, state| match state {
            TaskFetchState::TransientlyFailed { at, .. } => {
                at.elapsed() < TRANSIENT_FETCH_FAILURE_COOLDOWN
            }
            TaskFetchState::PermanentlyFailed { at, .. } => {
                at.elapsed() < PERMANENT_FETCH_FAILURE_COOLDOWN
            }
            TaskFetchState::InFlight => true,
        });

        let task_id_clone = *task_id;
        self.task_fetch_state.insert(
            task_id_clone,
            TaskFetchState::PermanentlyFailed {
                at: Instant::now(),
                message: "Cloud task data is unavailable in local conversation storage mode."
                    .to_string(),
            },
        );
        ctx.emit(AgentConversationsModelEvent::TasksUpdated);

        None
    }

    /// Returns all (name, uid) pairs for creators of tasks in the model.
    ///
    /// We use this function to populate the available creator filter list
    /// based on the tasks we have.
    pub fn get_all_creators(&self, app: &AppContext) -> Vec<(String, String)> {
        let mut creators: Vec<(String, String)> = self
            .tasks
            .values()
            .filter_map(|task| {
                let name = entry::task_creator_name(task, app)?;
                let uid = entry::task_creator_uid(task)?;
                Some((name, uid))
            })
            .collect();

        // Include the current user since they may have local conversations
        let auth_state = AuthStateProvider::as_ref(app).get();
        if let (Some(name), Some(uid)) = (auth_state.display_name(), auth_state.user_id()) {
            creators.push((name, uid.to_string()));
        }

        creators.sort_by(|a, b| a.0.cmp(&b.0));
        creators.dedup_by(|a, b| a.0 == b.0);

        creators
    }

    /// Returns a mapping of environment IDs to display names.
    ///
    /// When multiple environments share the same name, each is disambiguated
    /// as "<name> (<id>)".
    pub fn get_all_environment_ids_and_names(&self, ctx: &AppContext) -> HashMap<String, String> {
        let mut envs = HashMap::<String, String>::new();

        for task in self.tasks.values() {
            let Some(environment_id) = task
                .agent_config_snapshot
                .as_ref()
                .and_then(|s| s.environment_id.as_deref())
            else {
                continue;
            };

            let Some(server_id) = ServerId::try_from(environment_id).ok() else {
                continue;
            };
            let sync_id = SyncId::ServerId(server_id);
            let Some(env) = CloudAmbientAgentEnvironment::get_by_id(&sync_id, ctx) else {
                continue;
            };
            let env_model = &env.model().string_model;
            envs.insert(environment_id.to_string(), env_model.name.clone());
        }

        // Disambiguate duplicate names by appending the environment ID.
        let mut name_counts = HashMap::<String, usize>::new();
        for name in envs.values() {
            *name_counts.entry(name.clone()).or_default() += 1;
        }
        for (id, name) in &mut envs {
            if name_counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
                *name = format!("{name} ({id})");
            }
        }

        envs
    }

    /// Refreshes local conversations when user changes filters in AgentManagementView.
    pub fn fetch_tasks_for_filters(
        &mut self,
        _filters: &AgentManagementFilters,
        _current_user_uid: &str,
        ctx: &mut ModelContext<Self>,
    ) {
        self.sync_conversations(ctx);
    }

    /// Enforces cap on tasks stored in the model so it doesn't grow without bound.
    /// We always keep at least 200 personal tasks around so an influx of team tasks
    /// doesn't result in evicting personal task data.
    #[cfg(test)]
    fn enforce_task_cap(&mut self, current_user_uid: &str) {
        let total_cap = MAX_PERSONAL_TASKS + MAX_TEAM_TASKS;
        if self.tasks.len() <= total_cap {
            return;
        }

        let (mut personal, mut team): (Vec<_>, Vec<_>) =
            self.tasks.drain().partition(|(_, task)| {
                task.creator
                    .as_ref()
                    .is_some_and(|c| c.uid == current_user_uid)
            });

        // Sort each by updated_at (newest first), truncate
        personal.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
        team.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
        personal.truncate(MAX_PERSONAL_TASKS);
        team.truncate(MAX_TEAM_TASKS);

        self.tasks = personal.into_iter().chain(team).collect();
    }

    /// Clears all stored conversation and task data in memory.
    /// This is used when logging out to ensure no conversation history persists across users.
    pub(crate) fn reset(&mut self) {
        self.tasks.clear();
        self.conversations.clear();
        self.abort_existing_poll();
        self.abort_rtc_task_refresh_throttle();
        self.active_data_consumers_per_window.clear();
        self.task_fetch_state.clear();
        // Reset the initial load flag so that we can retry the initial sync with the new logged in user
        self.has_finished_initial_load = false;
    }
}

#[cfg(test)]
#[path = "agent_conversations_model_tests.rs"]
mod tests;
