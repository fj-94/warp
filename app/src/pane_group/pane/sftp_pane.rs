use warpui::{AppContext, ModelHandle, View, ViewContext, ViewHandle};

use crate::app_state::{LeafContents, SftpPaneSnapshot};
use crate::sftp_view::{SftpView, SftpViewEvent};

use super::{
    view::PaneView, DetachType, PaneConfiguration, PaneContent, PaneGroup, PaneId, ShareableLink,
    ShareableLinkError,
};

pub struct SftpPane {
    view: ViewHandle<PaneView<SftpView>>,
    pane_configuration: ModelHandle<PaneConfiguration>,
}

impl SftpPane {
    pub fn from_view(sftp_view: ViewHandle<SftpView>, ctx: &mut AppContext) -> Self {
        let pane_configuration = sftp_view.as_ref(ctx).pane_configuration();
        let view = ctx.add_typed_action_view(sftp_view.window_id(ctx), |ctx| {
            let pane_id = PaneId::from_sftp_pane_ctx(ctx);
            PaneView::new(pane_id, sftp_view, (), pane_configuration.clone(), ctx)
        });
        Self {
            view,
            pane_configuration,
        }
    }

    pub fn new<V: View>(ctx: &mut ViewContext<V>) -> Self {
        let view = ctx.add_typed_action_view(SftpView::new);
        Self::from_view(view, ctx)
    }

    pub fn from_snapshot<V: View>(snapshot: SftpPaneSnapshot, ctx: &mut ViewContext<V>) -> Self {
        let view = ctx.add_typed_action_view(move |ctx| SftpView::from_snapshot(snapshot, ctx));
        Self::from_view(view, ctx)
    }

    pub fn sftp_view(&self, ctx: &AppContext) -> ViewHandle<SftpView> {
        self.view.as_ref(ctx).child(ctx)
    }
}

impl PaneContent for SftpPane {
    fn id(&self) -> PaneId {
        PaneId::from_sftp_pane_view(&self.view)
    }

    fn attach(
        &self,
        _group: &PaneGroup,
        focus_handle: crate::pane_group::focus_state::PaneFocusHandle,
        ctx: &mut ViewContext<PaneGroup>,
    ) {
        self.view
            .update(ctx, |view, ctx| view.set_focus_handle(focus_handle, ctx));

        let sftp_view = self.sftp_view(ctx);
        let pane_id = self.id();
        ctx.subscribe_to_view(
            &sftp_view,
            move |pane_group, sftp_view, event, ctx| match event {
                SftpViewEvent::Pane(pane_event) => {
                    pane_group.handle_pane_event(pane_id, pane_event, ctx)
                }
                SftpViewEvent::ConnectRequested => {
                    let target = pane_group
                        .active_session_view(ctx)
                        .and_then(|terminal| terminal.as_ref(ctx).active_session_sftp_target(ctx));
                    sftp_view.update(ctx, |view, ctx| {
                        view.set_current_tab_target(target);
                        view.connect_from_editor(ctx);
                    });
                }
                #[cfg(feature = "local_fs")]
                SftpViewEvent::OpenFile(_) => {}
            },
        );
        ctx.subscribe_to_view(&self.view, move |group, _, event, ctx| {
            group.handle_pane_view_event(pane_id, event, ctx);
        });
    }

    fn detach(
        &self,
        _group: &PaneGroup,
        detach_type: DetachType,
        ctx: &mut ViewContext<PaneGroup>,
    ) {
        let sftp_view = self.sftp_view(ctx);
        if matches!(detach_type, DetachType::Closed) {
            sftp_view.update(ctx, |view, _ctx| view.shutdown());
        }
        ctx.unsubscribe_to_view(&sftp_view);
        ctx.unsubscribe_to_view(&self.view);
    }

    fn snapshot(&self, app: &AppContext) -> LeafContents {
        let snapshot = self
            .sftp_view(app)
            .read(app, |view, ctx| view.snapshot(ctx));
        LeafContents::Sftp(snapshot)
    }

    fn has_application_focus(&self, ctx: &mut ViewContext<PaneGroup>) -> bool {
        self.view.is_self_or_child_focused(ctx)
    }

    fn focus(&self, ctx: &mut ViewContext<PaneGroup>) {
        self.sftp_view(ctx).update(ctx, |view, ctx| view.focus(ctx));
    }

    fn shareable_link(
        &self,
        _ctx: &mut ViewContext<PaneGroup>,
    ) -> Result<ShareableLink, ShareableLinkError> {
        Ok(ShareableLink::Base)
    }

    fn pane_configuration(&self) -> ModelHandle<PaneConfiguration> {
        self.pane_configuration.clone()
    }

    fn is_pane_being_dragged(&self, ctx: &AppContext) -> bool {
        self.view.as_ref(ctx).is_being_dragged()
    }
}
