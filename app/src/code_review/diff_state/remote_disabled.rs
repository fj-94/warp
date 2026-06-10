#![allow(dead_code)]

use crate::util::git::{BranchEntry, Commit, PrInfo};
use warp_core::SessionId;
use warp_util::remote_path::RemotePath;
use warpui::ModelContext;

use super::{DiffMode, DiffState, DiffStateModelEvent, DiffStats, FileStatusInfo};

pub struct RemoteDiffStateModel {
    remote_path: RemotePath,
    mode: DiffMode,
    session_id: SessionId,
}

impl warpui::Entity for RemoteDiffStateModel {
    type Event = DiffStateModelEvent;
}

impl RemoteDiffStateModel {
    pub fn new(
        remote_path: RemotePath,
        mode: DiffMode,
        session_id: SessionId,
        _ctx: &mut ModelContext<Self>,
    ) -> Self {
        Self {
            remote_path,
            mode,
            session_id,
        }
    }

    pub(crate) fn replay_latest_diffs(&self, ctx: &mut ModelContext<Self>) {
        ctx.emit(DiffStateModelEvent::ConnectionLost);
    }

    #[cfg(feature = "local_fs")]
    pub fn unsubscribe(&self, _ctx: &mut ModelContext<Self>) {}

    pub fn get(&self) -> DiffState {
        DiffState::Loaded
    }

    pub fn diff_mode(&self) -> DiffMode {
        self.mode.clone()
    }

    pub fn get_uncommitted_stats(&self) -> Option<DiffStats> {
        None
    }

    pub fn get_main_branch_name(&self) -> Option<String> {
        None
    }

    pub fn get_current_branch_name(&self) -> Option<String> {
        None
    }

    pub fn is_on_main_branch(&self) -> bool {
        false
    }

    pub fn unpushed_commits(&self) -> &[Commit] {
        &[]
    }

    pub fn upstream_ref(&self) -> Option<&str> {
        None
    }

    pub fn upstream_differs_from_main(&self) -> bool {
        false
    }

    pub fn pr_info(&self) -> Option<&PrInfo> {
        None
    }

    pub fn is_pr_info_refreshing(&self) -> bool {
        false
    }

    pub fn is_git_operation_blocked(&self, _ctx: &warpui::AppContext) -> bool {
        true
    }

    pub fn has_head(&self) -> bool {
        false
    }

    pub fn remote_path(&self) -> RemotePath {
        self.remote_path.clone()
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn set_diff_mode(&mut self, mode: DiffMode, _ctx: &mut ModelContext<Self>) {
        self.mode = mode;
    }

    pub fn fetch_branches(&self, ctx: &mut ModelContext<Self>) {
        ctx.emit(DiffStateModelEvent::BranchesReceived(
            Vec::<BranchEntry>::new(),
        ));
    }

    pub fn discard_files(
        &self,
        _file_infos: Vec<FileStatusInfo>,
        _should_stash: bool,
        _branch_name: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        ctx.emit(DiffStateModelEvent::ConnectionLost);
    }
}
