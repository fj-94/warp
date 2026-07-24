//! SFTP file browser and transfer queue.

#[cfg(feature = "local_fs")]
use std::collections::HashMap;
use std::{borrow::Cow, collections::BTreeSet, path::PathBuf, time::SystemTime};

use async_channel::unbounded;
use chrono::{DateTime, Local};
use pathfinder_geometry::vector::Vector2F;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use warp_core::ui::theme::color::internal_colors;
use warpui::{
    elements::{
        ChildAnchor, ChildView, ClippedScrollable, ConstrainedBox, Container, CrossAxisAlignment,
        Element, Fill, Flex, Hoverable, MainAxisSize, MouseStateHandle, OffsetPositioning,
        ParentAnchor, ParentElement, ParentOffsetBounds, SavePosition, ScrollbarWidth, Shrinkable,
        Stack, Text, Wrap,
    },
    event::ModifiersState,
    platform::Cursor,
    ui_components::components::{Coords, UiComponent, UiComponentStyles},
    ui_components::text_input::TextInput,
    AppContext, Entity, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle,
};

use crate::{
    appearance::Appearance,
    editor::{EditorOptions, EditorView, Event as EditorEvent, InteractionState, TextOptions},
    menu::{Menu, MenuItem, MenuItemFields},
    pane_group::{
        focus_state::PaneFocusHandle,
        pane::view::{self, HeaderContent, StandardHeader, StandardHeaderOptions},
        BackingView, PaneConfiguration, PaneEvent,
    },
    ui_components::icons::Icon,
    view_components::action_button::{ActionButton, ButtonSize, PrimaryTheme, SecondaryTheme},
};

use crate::sftp::{
    load_ssh_host_aliases, parent_remote_path, RemoteEntry, RemoteFileType, Result as SftpResult,
    SftpConnection, SftpError, TransferProgress,
};

pub const SFTP_HEADER_TEXT: &str = "SFTP";

#[derive(Debug, Clone)]
pub enum SftpViewAction {
    Connect,
    CancelConnect,
    Refresh,
    NavigateUp,
    SelectAlias(String),
    SelectEntry {
        index: usize,
        modifiers: ModifiersState,
    },
    OpenContextMenu {
        position: Vector2F,
        index: usize,
    },
    OpenDirectoryContextMenu {
        position: Vector2F,
    },
    OpenEntry(String),
    EditEntry(String),
    RenameEntry(String),
    StartCreateDirectory {
        parent: String,
        after_path: Option<String>,
    },
    StartCreateFile {
        parent: String,
        after_path: Option<String>,
    },
    DeleteEntry(String),
    DownloadEntry(String),
    OpenUploadPicker,
    OpenDownloadPicker,
    UploadPaths(Vec<PathBuf>),
    DownloadDirectory(PathBuf),
    FilePickerError(String),
    CancelTransfer(Uuid),
    RetryTransfer(Uuid),
    ReplaceTransfer(Uuid),
    CancelAllTransfers,
}

#[derive(Debug, Clone)]
pub enum SftpViewEvent {
    Pane(PaneEvent),
    #[cfg(feature = "local_fs")]
    OpenFile(PathBuf),
}

struct ContextMenuState {
    position: Vector2F,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingCreationKind {
    Directory,
    File,
}

struct PendingCreation {
    parent: String,
    after_path: Option<String>,
    kind: PendingCreationKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SelectionModifiers {
    additive: bool,
    range: bool,
}

impl From<ModifiersState> for SelectionModifiers {
    fn from(modifiers: ModifiersState) -> Self {
        Self {
            additive: modifiers.ctrl || modifiers.cmd,
            range: modifiers.shift,
        }
    }
}

#[cfg(feature = "local_fs")]
#[derive(Clone)]
struct RemoteEdit {
    target: String,
    remote_path: String,
}

#[derive(Clone, Debug)]
enum TransferOperation {
    Upload {
        local_path: PathBuf,
        remote_directory: String,
    },
    Download {
        remote_path: String,
        local_directory: PathBuf,
    },
}

async fn execute_transfer(
    connection: &SftpConnection,
    operation: TransferOperation,
    overwrite: bool,
    cancellation: CancellationToken,
    progress: async_channel::Sender<TransferProgress>,
) -> SftpResult<()> {
    match operation {
        TransferOperation::Upload {
            local_path,
            remote_directory,
        } => {
            connection
                .upload_file(
                    &local_path,
                    &remote_directory,
                    overwrite,
                    cancellation,
                    progress,
                )
                .await
        }
        TransferOperation::Download {
            remote_path,
            local_directory,
        } => {
            connection
                .download_file(
                    &remote_path,
                    &local_directory,
                    overwrite,
                    cancellation,
                    progress,
                )
                .await
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferStatus {
    Running,
    Completed,
    Cancelled,
    AwaitingReplacement,
    Failed,
}

struct TransferItem {
    id: Uuid,
    name: String,
    operation: TransferOperation,
    status: TransferStatus,
    transferred: u64,
    total: Option<u64>,
    error: Option<String>,
    cancellation: CancellationToken,
    attempt: u64,
    cancel_button: ViewHandle<ActionButton>,
    retry_button: ViewHandle<ActionButton>,
    replace_button: ViewHandle<ActionButton>,
}

pub struct SftpView {
    pane_configuration: warpui::ModelHandle<PaneConfiguration>,
    target_editor: ViewHandle<EditorView>,
    path_editor: ViewHandle<EditorView>,
    rename_editor: ViewHandle<EditorView>,
    context_menu: ViewHandle<Menu<SftpViewAction>>,
    context_menu_state: Option<ContextMenuState>,
    position_id: String,
    pending_rename: Option<String>,
    pending_creation: Option<PendingCreation>,
    focus_handle: Option<PaneFocusHandle>,
    aliases: Vec<ViewHandle<ActionButton>>,
    connect_button: ViewHandle<ActionButton>,
    cancel_connect_button: ViewHandle<ActionButton>,
    navigate_up_button: ViewHandle<ActionButton>,
    refresh_button: ViewHandle<ActionButton>,
    upload_button: ViewHandle<ActionButton>,
    download_button: ViewHandle<ActionButton>,
    cancel_all_button: ViewHandle<ActionButton>,
    connection: Option<SftpConnection>,
    target: String,
    remote_path: String,
    entries: Vec<RemoteEntry>,
    selected_paths: BTreeSet<String>,
    selection_anchor: Option<usize>,
    empty_directory_mouse_state: MouseStateHandle,
    row_mouse_states: Vec<MouseStateHandle>,
    directory_scroll_state: warpui::elements::ClippedScrollStateHandle,
    transfer_scroll_state: warpui::elements::ClippedScrollStateHandle,
    transfers: Vec<TransferItem>,
    is_connecting: bool,
    is_loading: bool,
    is_opening_file: bool,
    error: Option<String>,
    connection_generation: u64,
    directory_generation: u64,
    connection_cancellation: CancellationToken,
    #[cfg(feature = "local_fs")]
    cache_root: PathBuf,
    #[cfg(feature = "local_fs")]
    open_remote_files: HashMap<PathBuf, RemoteEdit>,
}

impl SftpView {
    fn new_action_button(
        label: impl Into<Cow<'static, str>>,
        icon: Icon,
        action: SftpViewAction,
        primary: bool,
        ctx: &mut ViewContext<Self>,
    ) -> ViewHandle<ActionButton> {
        let label = label.into();
        ctx.add_typed_action_view(move |_| {
            let button = if primary {
                ActionButton::new(label.clone(), PrimaryTheme)
            } else {
                ActionButton::new(label.clone(), SecondaryTheme)
            };
            button
                .with_icon(icon)
                .with_size(ButtonSize::Small)
                .on_click(move |ctx| ctx.dispatch_typed_action(action.clone()))
        })
    }

    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let target_editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            let mut editor = EditorView::new(
                EditorOptions {
                    single_line: true,
                    soft_wrap: false,
                    text: TextOptions::ui_font_size(appearance),
                    ..Default::default()
                },
                ctx,
            );
            editor.set_placeholder_text("user@host or SSH alias", ctx);
            editor.set_buffer_text("", ctx);
            editor
        });
        let path_editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            let mut editor = EditorView::new(
                EditorOptions {
                    single_line: true,
                    soft_wrap: false,
                    text: TextOptions::ui_font_size(appearance),
                    ..Default::default()
                },
                ctx,
            );
            editor.set_placeholder_text("Remote path", ctx);
            editor.set_interaction_state(InteractionState::Disabled, ctx);
            editor
        });
        let rename_editor = ctx.add_typed_action_view(|ctx| {
            let appearance = Appearance::as_ref(ctx);
            EditorView::new(
                EditorOptions {
                    single_line: true,
                    soft_wrap: false,
                    text: TextOptions::ui_font_size(appearance),
                    ..Default::default()
                },
                ctx,
            )
        });
        let context_menu = ctx.add_typed_action_view(|_| {
            Menu::new()
                .prevent_interaction_with_other_elements()
                .with_drop_shadow()
        });
        let connect_button =
            Self::new_action_button("Connect", Icon::Globe, SftpViewAction::Connect, true, ctx);
        let cancel_connect_button =
            Self::new_action_button("Cancel", Icon::X, SftpViewAction::CancelConnect, false, ctx);
        let navigate_up_button =
            Self::new_action_button("Up", Icon::ArrowUp, SftpViewAction::NavigateUp, false, ctx);
        let refresh_button = Self::new_action_button(
            "Refresh",
            Icon::Refresh,
            SftpViewAction::Refresh,
            false,
            ctx,
        );
        let upload_button = Self::new_action_button(
            "Upload",
            Icon::UploadCloud,
            SftpViewAction::OpenUploadPicker,
            true,
            ctx,
        );
        let download_button = Self::new_action_button(
            "Download",
            Icon::Download,
            SftpViewAction::OpenDownloadPicker,
            false,
            ctx,
        );
        let cancel_all_button = Self::new_action_button(
            "Cancel all",
            Icon::X,
            SftpViewAction::CancelAllTransfers,
            false,
            ctx,
        );
        let aliases = load_ssh_host_aliases()
            .into_iter()
            .take(12)
            .map(|alias| {
                Self::new_action_button(
                    alias.clone(),
                    Icon::Terminal,
                    SftpViewAction::SelectAlias(alias),
                    false,
                    ctx,
                )
            })
            .collect();

        ctx.subscribe_to_view(&target_editor, Self::handle_target_editor_event);
        ctx.subscribe_to_view(&path_editor, Self::handle_path_editor_event);
        ctx.subscribe_to_view(&rename_editor, Self::handle_rename_editor_event);
        ctx.subscribe_to_view(&context_menu, |view, _, event, ctx| {
            if matches!(event, crate::menu::Event::Close { .. }) {
                view.context_menu_state = None;
                ctx.notify();
            }
        });
        #[cfg(feature = "local_fs")]
        ctx.subscribe_to_model(
            &crate::code::global_buffer_model::GlobalBufferModel::handle(ctx),
            |view, _, event, ctx| view.handle_global_buffer_event(event, ctx),
        );

        Self {
            pane_configuration: ctx.add_model(|_ctx| PaneConfiguration::new(SFTP_HEADER_TEXT)),
            target_editor,
            path_editor,
            rename_editor,
            context_menu,
            context_menu_state: None,
            position_id: format!("sftp_view_{}", ctx.view_id()),
            pending_rename: None,
            pending_creation: None,
            focus_handle: None,
            aliases,
            connect_button,
            cancel_connect_button,
            navigate_up_button,
            refresh_button,
            upload_button,
            download_button,
            cancel_all_button,
            connection: None,
            target: String::new(),
            remote_path: String::new(),
            entries: Vec::new(),
            selected_paths: BTreeSet::new(),
            selection_anchor: None,
            empty_directory_mouse_state: MouseStateHandle::default(),
            row_mouse_states: Vec::new(),
            directory_scroll_state: Default::default(),
            transfer_scroll_state: Default::default(),
            transfers: Vec::new(),
            is_connecting: false,
            is_loading: false,
            is_opening_file: false,
            error: None,
            connection_generation: 0,
            directory_generation: 0,
            connection_cancellation: CancellationToken::new(),
            #[cfg(feature = "local_fs")]
            cache_root: std::env::temp_dir()
                .join("warp-sftp")
                .join(Uuid::new_v4().to_string()),
            #[cfg(feature = "local_fs")]
            open_remote_files: HashMap::new(),
        }
    }

    pub fn from_snapshot(
        snapshot: crate::app_state::SftpPaneSnapshot,
        ctx: &mut ViewContext<Self>,
    ) -> Self {
        let mut view = Self::new(ctx);
        if let Some(target) = snapshot.target {
            view.target_editor.update(ctx, |editor, ctx| {
                editor.set_buffer_text(&target, ctx);
            });
            view.connect(target, snapshot.remote_path, ctx);
        }
        view
    }

    pub fn pane_configuration(&self) -> warpui::ModelHandle<PaneConfiguration> {
        self.pane_configuration.clone()
    }

    pub fn snapshot(&self, ctx: &AppContext) -> crate::app_state::SftpPaneSnapshot {
        let target = self
            .target_editor
            .read(ctx, |editor, ctx| editor.buffer_text(ctx).trim().to_owned());
        crate::app_state::SftpPaneSnapshot {
            target: (!target.is_empty()).then_some(target),
            remote_path: (!self.remote_path.is_empty()).then(|| self.remote_path.clone()),
        }
    }

    pub fn connect_to_target(&mut self, target: String, ctx: &mut ViewContext<Self>) {
        if target == self.target && (self.connection.is_some() || self.is_connecting) {
            return;
        }
        self.target_editor.update(ctx, |editor, ctx| {
            editor.set_buffer_text(&target, ctx);
        });
        self.connect(target, None, ctx);
    }

    pub fn connect_to_target_if_empty(&mut self, target: String, ctx: &mut ViewContext<Self>) {
        let editor_is_empty = self
            .target_editor
            .read(ctx, |editor, ctx| editor.buffer_text(ctx).trim().is_empty());
        if editor_is_empty && self.connection.is_none() && !self.is_connecting {
            self.connect_to_target(target, ctx);
        }
    }

    pub fn shutdown(&mut self) {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.directory_generation = self.directory_generation.wrapping_add(1);
        self.connection_cancellation.cancel();
        self.cancel_all_transfers();
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
        self.entries.clear();
        self.selected_paths.clear();
        self.selection_anchor = None;
        self.row_mouse_states.clear();
        self.is_connecting = false;
        self.is_loading = false;
        self.is_opening_file = false;
    }

    pub fn focus(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.focus(&self.target_editor);
    }

    fn handle_target_editor_event(
        &mut self,
        _editor: ViewHandle<EditorView>,
        event: &EditorEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        if matches!(event, EditorEvent::Enter) {
            self.connect_from_editor(ctx);
        }
    }

    fn handle_path_editor_event(
        &mut self,
        _editor: ViewHandle<EditorView>,
        event: &EditorEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        if matches!(event, EditorEvent::Enter) && self.connection.is_some() {
            let path = self
                .path_editor
                .read(ctx, |editor, ctx| editor.buffer_text(ctx));
            self.load_directory(path, ctx);
        }
    }

    fn handle_rename_editor_event(
        &mut self,
        _editor: ViewHandle<EditorView>,
        event: &EditorEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        match event {
            EditorEvent::Enter => self.commit_pending_edit(ctx),
            EditorEvent::Escape => self.cancel_pending_edit(ctx),
            _ => {}
        }
    }

    #[cfg(feature = "local_fs")]
    fn handle_global_buffer_event(
        &mut self,
        event: &crate::code::global_buffer_model::GlobalBufferModelEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        let crate::code::global_buffer_model::GlobalBufferModelEvent::FileSaved { file_id } = event
        else {
            return;
        };
        let Some(local_path) = crate::code::global_buffer_model::GlobalBufferModel::as_ref(ctx)
            .file_path(*file_id)
            .map(PathBuf::from)
        else {
            return;
        };
        let Some(remote) = self.open_remote_files.get(&local_path).cloned() else {
            return;
        };
        if remote.target != self.target || self.connection.is_none() {
            self.error = Some(format!(
                "Reconnect to {} before saving {}",
                remote.target, remote.remote_path
            ));
            ctx.notify();
            return;
        }
        let Some(name) = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            return;
        };
        self.add_transfer(
            name,
            TransferOperation::Upload {
                local_path,
                remote_directory: parent_remote_path(&remote.remote_path),
            },
            true,
            ctx,
        );
    }

    fn connect_from_editor(&mut self, ctx: &mut ViewContext<Self>) {
        let target = self
            .target_editor
            .read(ctx, |editor, ctx| editor.buffer_text(ctx).trim().to_owned());
        self.connect(target, None, ctx);
    }

    fn cancel_connect(&mut self, ctx: &mut ViewContext<Self>) {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.connection_cancellation.cancel();
        self.is_connecting = false;
        self.error = Some("Connection cancelled".to_string());
        ctx.notify();
    }

    fn connect(
        &mut self,
        target: String,
        requested_path: Option<String>,
        ctx: &mut ViewContext<Self>,
    ) {
        if target.is_empty() {
            self.error = Some("Enter an SSH host or alias".to_string());
            ctx.notify();
            return;
        }

        self.connection_cancellation.cancel();
        self.connection_cancellation = CancellationToken::new();
        let connection_cancellation = self.connection_cancellation.clone();
        self.cancel_all_transfers();
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
        self.entries.clear();
        self.selected_paths.clear();
        self.selection_anchor = None;
        self.target = target.clone();
        self.remote_path.clear();
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let connection_generation = self.connection_generation;
        self.error = None;
        self.is_connecting = true;
        self.is_loading = false;
        self.path_editor.update(ctx, |editor, ctx| {
            editor.set_interaction_state(InteractionState::Disabled, ctx);
            editor.clear_buffer(ctx);
        });
        ctx.notify();

        ctx.spawn(
            async move {
                let connection = SftpConnection::connect(target, connection_cancellation).await?;
                let path = match requested_path {
                    Some(path) => connection.canonicalize(&path).await?,
                    None => connection.initial_directory().await?,
                };
                Ok::<_, SftpError>((connection, path))
            },
            move |view, result, ctx| {
                if view.connection_generation != connection_generation {
                    return;
                }
                view.is_connecting = false;
                match result {
                    Ok((connection, path)) => {
                        view.connection = Some(connection);
                        view.error = None;
                        view.remote_path = path.clone();
                        view.path_editor.update(ctx, |editor, ctx| {
                            editor.set_interaction_state(InteractionState::Editable, ctx);
                            editor.set_buffer_text(&path, ctx);
                        });
                        view.load_directory(path, ctx);
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                ctx.notify();
            },
        );
    }

    fn load_directory(&mut self, path: String, ctx: &mut ViewContext<Self>) {
        let Some(connection) = self.connection.clone() else {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        };
        let target = self.target.clone();
        let cancellation = self.connection_cancellation.clone();
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let directory_generation = self.directory_generation;
        self.is_loading = true;
        self.error = None;
        ctx.notify();

        ctx.spawn(
            async move {
                let first_result = async {
                    let path = connection.canonicalize(&path).await?;
                    let entries = connection.list_dir(&path).await?;
                    Ok::<_, SftpError>((path, entries))
                }
                .await;
                match first_result {
                    Ok((path, entries)) => Ok((connection, path, entries)),
                    Err(error) if connection.is_transport_failure(&error) => {
                        connection.close();
                        let replacement = SftpConnection::connect(target, cancellation).await?;
                        let path = replacement.canonicalize(&path).await?;
                        let entries = replacement.list_dir(&path).await?;
                        Ok((replacement, path, entries))
                    }
                    Err(error) => Err(error),
                }
            },
            move |view, result, ctx| {
                if view.directory_generation != directory_generation {
                    return;
                }
                view.is_loading = false;
                match result {
                    Ok((connection, path, entries)) => {
                        view.connection = Some(connection);
                        view.set_directory(path, entries, ctx);
                        view.error = None;
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                ctx.notify();
            },
        );
    }

    fn set_directory(
        &mut self,
        path: String,
        entries: Vec<RemoteEntry>,
        ctx: &mut ViewContext<Self>,
    ) {
        self.remote_path = path.clone();
        self.entries = entries;
        self.selected_paths.clear();
        self.selection_anchor = None;
        self.pending_rename = None;
        self.pending_creation = None;
        self.row_mouse_states = (0..self.entries.len())
            .map(|_| MouseStateHandle::default())
            .collect();
        self.path_editor.update(ctx, |editor, ctx| {
            editor.set_interaction_state(InteractionState::Editable, ctx);
            editor.set_buffer_text(&path, ctx);
        });
    }

    fn select_entry(
        &mut self,
        index: usize,
        modifiers: ModifiersState,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let modifiers = SelectionModifiers::from(modifiers);
        if entry.file_type == RemoteFileType::Directory
            && modifiers == SelectionModifiers::default()
        {
            self.open_entry(entry.path, ctx);
            return;
        }
        self.context_menu_state = None;
        let paths = self
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        update_selection(
            &paths,
            &mut self.selected_paths,
            &mut self.selection_anchor,
            index,
            modifiers,
        );
        ctx.notify();
    }

    fn open_entry(&mut self, path: String, ctx: &mut ViewContext<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };
        self.context_menu_state = None;
        self.selected_paths.clear();
        self.selected_paths.insert(entry.path.clone());
        self.selection_anchor = self.entries.iter().position(|item| item.path == entry.path);
        if entry.file_type == RemoteFileType::Directory {
            self.load_directory(entry.path, ctx);
        }
        ctx.notify();
    }

    fn edit_entry(&mut self, path: String, ctx: &mut ViewContext<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path && entry.file_type != RemoteFileType::Directory)
            .cloned()
        else {
            return;
        };
        self.context_menu_state = None;
        self.selected_paths.clear();
        self.selected_paths.insert(entry.path.clone());
        self.selection_anchor = self.entries.iter().position(|item| item.path == entry.path);
        self.open_remote_file(entry, ctx);
        ctx.notify();
    }

    fn open_context_menu(&mut self, position: Vector2F, index: usize, ctx: &mut ViewContext<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if !self.selected_paths.contains(&entry.path) {
            self.selected_paths.clear();
            self.selected_paths.insert(entry.path.clone());
            self.selection_anchor = Some(index);
        }
        self.context_menu_state = Some(ContextMenuState { position });
        let mut items = if entry.file_type == RemoteFileType::Directory {
            vec![MenuItemFields::new("Open")
                .with_icon(Icon::Folder)
                .with_on_select_action(SftpViewAction::OpenEntry(entry.path.clone()))
                .into_item()]
        } else {
            vec![MenuItemFields::new("Edit")
                .with_icon(Icon::Pencil)
                .with_on_select_action(SftpViewAction::EditEntry(entry.path.clone()))
                .into_item()]
        };
        if entry.file_type != RemoteFileType::Directory {
            items.push(
                MenuItemFields::new("Download")
                    .with_icon(Icon::Download)
                    .with_on_select_action(SftpViewAction::DownloadEntry(entry.path.clone()))
                    .into_item(),
            );
        }
        items.push(MenuItem::Separator);
        let create_parent = if entry.file_type == RemoteFileType::Directory {
            entry.path.clone()
        } else {
            self.remote_path.clone()
        };
        items.extend([
            MenuItemFields::new("New folder")
                .with_icon(Icon::Folder)
                .with_on_select_action(SftpViewAction::StartCreateDirectory {
                    parent: create_parent.clone(),
                    after_path: Some(entry.path.clone()),
                })
                .into_item(),
            MenuItemFields::new("New file")
                .with_icon(Icon::File)
                .with_on_select_action(SftpViewAction::StartCreateFile {
                    parent: create_parent,
                    after_path: Some(entry.path.clone()),
                })
                .into_item(),
            MenuItem::Separator,
        ]);
        items.extend([
            MenuItemFields::new("Rename")
                .with_icon(Icon::Rename)
                .with_on_select_action(SftpViewAction::RenameEntry(entry.path.clone()))
                .into_item(),
            MenuItemFields::new("Delete")
                .with_icon(Icon::Trash)
                .with_on_select_action(SftpViewAction::DeleteEntry(entry.path))
                .into_item(),
        ]);
        self.context_menu.update(ctx, move |menu, ctx| {
            menu.set_items(items, ctx);
            ctx.notify();
        });
        ctx.notify();
    }

    fn open_directory_context_menu(&mut self, position: Vector2F, ctx: &mut ViewContext<Self>) {
        self.context_menu_state = Some(ContextMenuState { position });
        let parent = self.remote_path.clone();
        let items = vec![
            MenuItemFields::new("New folder")
                .with_icon(Icon::Folder)
                .with_on_select_action(SftpViewAction::StartCreateDirectory {
                    parent: parent.clone(),
                    after_path: None,
                })
                .into_item(),
            MenuItemFields::new("New file")
                .with_icon(Icon::File)
                .with_on_select_action(SftpViewAction::StartCreateFile {
                    parent,
                    after_path: None,
                })
                .into_item(),
        ];
        self.context_menu.update(ctx, move |menu, ctx| {
            menu.set_items(items, ctx);
            ctx.notify();
        });
        ctx.notify();
    }

    fn start_rename(&mut self, path: String, ctx: &mut ViewContext<Self>) {
        let Some(entry) = self.entries.iter().find(|entry| entry.path == path) else {
            return;
        };
        self.context_menu_state = None;
        self.pending_creation = None;
        self.pending_rename = Some(path);
        self.rename_editor.update(ctx, |editor, ctx| {
            editor.set_buffer_text(&entry.name, ctx);
            editor.select_all(ctx);
        });
        ctx.focus(&self.rename_editor);
        ctx.notify();
    }

    fn start_create(
        &mut self,
        parent: String,
        after_path: Option<String>,
        kind: PendingCreationKind,
        ctx: &mut ViewContext<Self>,
    ) {
        self.context_menu_state = None;
        self.pending_rename = None;
        self.pending_creation = Some(PendingCreation {
            parent,
            after_path,
            kind,
        });
        self.rename_editor.update(ctx, |editor, ctx| {
            editor.clear_buffer(ctx);
        });
        ctx.focus(&self.rename_editor);
        ctx.notify();
    }

    fn cancel_pending_edit(&mut self, ctx: &mut ViewContext<Self>) {
        self.pending_rename = None;
        self.pending_creation = None;
        self.rename_editor.update(ctx, |editor, ctx| {
            editor.clear_buffer(ctx);
        });
        ctx.notify();
    }

    fn commit_pending_edit(&mut self, ctx: &mut ViewContext<Self>) {
        if self.pending_rename.is_some() {
            self.commit_rename(ctx);
        } else if self.pending_creation.is_some() {
            self.commit_create(ctx);
        }
    }

    fn commit_rename(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(from) = self.pending_rename.take() else {
            return;
        };
        let name = self.rename_editor.read(ctx, |editor, ctx| {
            editor.buffer_text(ctx).trim().to_string()
        });
        self.rename_editor.update(ctx, |editor, ctx| {
            editor.clear_buffer(ctx);
        });
        if !is_valid_remote_name(&name) {
            self.error = Some("Enter a valid file name".to_string());
            ctx.notify();
            return;
        }
        let Some(connection) = self.connection.clone() else {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        };
        let to = crate::sftp::join_remote_path(&parent_remote_path(&from), &name);
        #[cfg(feature = "local_fs")]
        let from_for_state = from.clone();
        #[cfg(feature = "local_fs")]
        let to_for_state = to.clone();
        let current_path = self.remote_path.clone();
        self.is_loading = true;
        self.error = None;
        ctx.spawn(
            async move { connection.rename(&from, &to).await },
            move |view, result, ctx| {
                view.is_loading = false;
                match result {
                    Ok(()) => {
                        #[cfg(feature = "local_fs")]
                        for remote in view.open_remote_files.values_mut() {
                            if remote.target == view.target
                                && (remote.remote_path == from_for_state
                                    || remote
                                        .remote_path
                                        .starts_with(&format!("{from_for_state}/")))
                            {
                                remote.remote_path =
                                    remote
                                        .remote_path
                                        .replacen(&from_for_state, &to_for_state, 1);
                            }
                        }
                        view.load_directory(current_path, ctx)
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        ctx.notify();
                    }
                }
            },
        );
    }

    fn commit_create(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(pending) = self.pending_creation.take() else {
            return;
        };
        let name = self.rename_editor.read(ctx, |editor, ctx| {
            editor.buffer_text(ctx).trim().to_string()
        });
        self.rename_editor.update(ctx, |editor, ctx| {
            editor.clear_buffer(ctx);
        });
        if !is_valid_remote_name(&name) {
            self.error = Some("Enter a valid file name".to_string());
            ctx.notify();
            return;
        }
        let Some(connection) = self.connection.clone() else {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        };
        let path = crate::sftp::join_remote_path(&pending.parent, &name);
        let current_path = pending.parent.clone();
        self.is_loading = true;
        self.error = None;
        ctx.spawn(
            async move {
                match pending.kind {
                    PendingCreationKind::Directory => connection.create_directory(&path).await,
                    PendingCreationKind::File => connection.create_file(&path).await,
                }
            },
            move |view, result, ctx| {
                view.is_loading = false;
                match result {
                    Ok(()) => view.load_directory(current_path, ctx),
                    Err(error) => {
                        view.error = Some(error.to_string());
                        ctx.notify();
                    }
                }
            },
        );
    }

    fn delete_entry(&mut self, path: String, ctx: &mut ViewContext<Self>) {
        self.context_menu_state = None;
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .cloned()
        else {
            return;
        };
        let Some(connection) = self.connection.clone() else {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        };
        let current_path = self.remote_path.clone();
        #[cfg(feature = "local_fs")]
        let deleted_path = entry.path.clone();
        self.is_loading = true;
        self.error = None;
        ctx.spawn(
            async move { connection.delete(&entry.path, entry.file_type).await },
            move |view, result, ctx| {
                view.is_loading = false;
                match result {
                    Ok(()) => {
                        #[cfg(feature = "local_fs")]
                        view.open_remote_files.retain(|_, remote| {
                            remote.target != view.target
                                || (remote.remote_path != deleted_path
                                    && !remote.remote_path.starts_with(&format!("{deleted_path}/")))
                        });
                        view.load_directory(current_path, ctx)
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        ctx.notify();
                    }
                }
            },
        );
    }

    #[cfg(feature = "local_fs")]
    fn open_remote_file(&mut self, entry: RemoteEntry, ctx: &mut ViewContext<Self>) {
        if let Some((local_path, _)) = self
            .open_remote_files
            .iter()
            .find(|(_, remote)| remote.target == self.target && remote.remote_path == entry.path)
        {
            ctx.emit(SftpViewEvent::OpenFile(local_path.clone()));
            return;
        }
        let Some(connection) = self.connection.clone() else {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        };
        let local_directory = self.cache_root.join(Uuid::new_v4().to_string());
        let local_path = local_directory.join(&entry.name);
        let target = self.target.clone();
        let remote_path = entry.path;
        let remote_path_for_task = remote_path.clone();
        self.is_opening_file = true;
        self.error = None;
        ctx.notify();
        ctx.spawn(
            async move {
                tokio::fs::create_dir_all(&local_directory).await?;
                let (progress, _progress_rx) = unbounded();
                connection
                    .download_file(
                        &remote_path_for_task,
                        &local_directory,
                        true,
                        CancellationToken::new(),
                        progress,
                    )
                    .await?;
                Ok::<_, SftpError>(local_path)
            },
            move |view, result, ctx| {
                view.is_opening_file = false;
                match result {
                    Ok(local_path) => {
                        view.open_remote_files.insert(
                            local_path.clone(),
                            RemoteEdit {
                                target,
                                remote_path,
                            },
                        );
                        view.error = None;
                        ctx.emit(SftpViewEvent::OpenFile(local_path));
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                ctx.notify();
            },
        );
    }

    #[cfg(not(feature = "local_fs"))]
    fn open_remote_file(&mut self, _entry: RemoteEntry, ctx: &mut ViewContext<Self>) {
        self.error = Some("Opening SFTP files requires local file access".to_string());
        ctx.notify();
    }

    fn open_upload_picker(&mut self, ctx: &mut ViewContext<Self>) {
        let window_id = ctx.window_id();
        let view_id = ctx.view_id();
        ctx.open_file_picker(
            move |result, ctx| match result {
                Ok(paths) if !paths.is_empty() => {
                    let paths = paths.into_iter().map(PathBuf::from).collect();
                    ctx.dispatch_typed_action_for_view(
                        window_id,
                        view_id,
                        &SftpViewAction::UploadPaths(paths),
                    )
                }
                Ok(_) => {}
                Err(error) => ctx.dispatch_typed_action_for_view(
                    window_id,
                    view_id,
                    &SftpViewAction::FilePickerError(error.to_string()),
                ),
            },
            warpui::platform::FilePickerConfiguration::new().allow_multi_select(),
        );
    }

    fn open_download_picker(&mut self, ctx: &mut ViewContext<Self>) {
        let window_id = ctx.window_id();
        let view_id = ctx.view_id();
        ctx.open_file_picker(
            move |result, ctx| match result {
                Ok(paths) => {
                    if let Some(path) = paths.into_iter().next() {
                        ctx.dispatch_typed_action_for_view(
                            window_id,
                            view_id,
                            &SftpViewAction::DownloadDirectory(PathBuf::from(path)),
                        );
                    }
                }
                Err(error) => ctx.dispatch_typed_action_for_view(
                    window_id,
                    view_id,
                    &SftpViewAction::FilePickerError(error.to_string()),
                ),
            },
            warpui::platform::FilePickerConfiguration::new().folders_only(),
        );
    }

    fn enqueue_uploads(&mut self, paths: Vec<PathBuf>, ctx: &mut ViewContext<Self>) {
        if self.connection.is_none() || self.remote_path.is_empty() {
            return;
        }
        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("upload")
                .to_string();
            self.add_transfer(
                name,
                TransferOperation::Upload {
                    local_path: path,
                    remote_directory: self.remote_path.clone(),
                },
                false,
                ctx,
            );
        }
    }

    fn enqueue_download(&mut self, local_directory: PathBuf, ctx: &mut ViewContext<Self>) {
        if self.connection.is_none() {
            self.error = Some("Connect to a host first".to_string());
            ctx.notify();
            return;
        }
        let selected = self
            .entries
            .iter()
            .filter(|entry| self.selected_paths.contains(&entry.path))
            .filter(|entry| entry.file_type != RemoteFileType::Directory)
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty() {
            self.error = Some("Select one or more remote files first".to_string());
            ctx.notify();
            return;
        }
        for entry in selected {
            self.add_transfer(
                entry.name.clone(),
                TransferOperation::Download {
                    remote_path: entry.path,
                    local_directory: local_directory.clone(),
                },
                false,
                ctx,
            );
        }
    }

    fn add_transfer(
        &mut self,
        name: String,
        operation: TransferOperation,
        overwrite: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        let id = Uuid::new_v4();
        let cancel_button = Self::new_action_button(
            "Cancel",
            Icon::X,
            SftpViewAction::CancelTransfer(id),
            false,
            ctx,
        );
        let retry_button = Self::new_action_button(
            "Retry",
            Icon::RefreshCw04,
            SftpViewAction::RetryTransfer(id),
            false,
            ctx,
        );
        let replace_button = Self::new_action_button(
            "Replace",
            Icon::Check,
            SftpViewAction::ReplaceTransfer(id),
            true,
            ctx,
        );
        self.transfers.push(TransferItem {
            id,
            name,
            operation,
            status: TransferStatus::Running,
            transferred: 0,
            total: None,
            error: None,
            cancellation: CancellationToken::new(),
            attempt: 0,
            cancel_button,
            retry_button,
            replace_button,
        });
        self.run_transfer(id, overwrite, ctx);
        ctx.notify();
    }

    fn run_transfer(&mut self, id: Uuid, overwrite: bool, ctx: &mut ViewContext<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let Some(item) = self.transfers.iter_mut().find(|item| item.id == id) else {
            return;
        };
        item.status = TransferStatus::Running;
        item.transferred = 0;
        item.total = None;
        item.error = None;
        item.cancellation = CancellationToken::new();
        item.attempt += 1;
        let attempt = item.attempt;
        let cancellation = item.cancellation.clone();
        let operation = item.operation.clone();
        let target = self.target.clone();
        let connection_generation = self.connection_generation;
        let (progress_tx, progress_rx) = unbounded::<TransferProgress>();

        ctx.spawn_stream_local(
            progress_rx,
            move |view, progress, ctx| {
                if let Some(item) = view
                    .transfers
                    .iter_mut()
                    .find(|item| item.id == id && item.attempt == attempt)
                {
                    item.transferred = progress.transferred;
                    item.total = progress.total;
                }
                ctx.notify();
            },
            |_view, _ctx| {},
        );

        ctx.spawn(
            async move {
                let result = execute_transfer(
                    &connection,
                    operation.clone(),
                    overwrite,
                    cancellation.clone(),
                    progress_tx.clone(),
                )
                .await;
                if result
                    .as_ref()
                    .is_err_and(|error| connection.is_transport_failure(error))
                {
                    connection.close();
                    let replacement = SftpConnection::connect(target, cancellation.clone()).await?;
                    let result = execute_transfer(
                        &replacement,
                        operation,
                        overwrite,
                        cancellation,
                        progress_tx,
                    )
                    .await;
                    Ok::<_, SftpError>((result, Some(replacement)))
                } else {
                    Ok((result, None))
                }
            },
            move |view, result, ctx| {
                let (result, replacement) = match result {
                    Ok(result) => result,
                    Err(error) => (Err(error), None),
                };
                if view.connection_generation == connection_generation {
                    if let Some(replacement) = replacement {
                        view.connection = Some(replacement);
                    }
                }
                view.finish_transfer(id, attempt, result, ctx);
            },
        );
    }

    fn finish_transfer(
        &mut self,
        id: Uuid,
        attempt: u64,
        result: SftpResult<()>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(item) = self
            .transfers
            .iter_mut()
            .find(|item| item.id == id && item.attempt == attempt)
        else {
            return;
        };
        if item.cancellation.is_cancelled() {
            item.status = TransferStatus::Cancelled;
            ctx.notify();
            return;
        }
        match result {
            Ok(()) => {
                item.status = TransferStatus::Completed;
                item.error = None;
                if let Some(total) = item.total {
                    item.transferred = total;
                }
                if matches!(item.operation, TransferOperation::Upload { .. }) {
                    self.load_directory(self.remote_path.clone(), ctx);
                }
            }
            Err(SftpError::Cancelled) => item.status = TransferStatus::Cancelled,
            Err(SftpError::DestinationExists(path)) => {
                item.status = TransferStatus::AwaitingReplacement;
                item.error = Some(format!("Already exists: {path}"));
            }
            Err(error) => {
                item.status = TransferStatus::Failed;
                item.error = Some(error.to_string());
            }
        }
        ctx.notify();
    }

    fn cancel_transfer(&mut self, id: Uuid, ctx: &mut ViewContext<Self>) {
        if let Some(item) = self.transfers.iter_mut().find(|item| item.id == id) {
            item.cancellation.cancel();
            item.status = TransferStatus::Cancelled;
            ctx.notify();
        }
    }

    fn cancel_all_transfers(&mut self) {
        for item in &mut self.transfers {
            if item.status == TransferStatus::Running {
                item.cancellation.cancel();
                item.status = TransferStatus::Cancelled;
            }
        }
    }

    fn transfer_progress(item: &TransferItem) -> String {
        match item.total {
            Some(total) if total > 0 => format!(
                "{} / {} ({:.0}%)",
                format_bytes(item.transferred),
                format_bytes(total),
                item.transferred as f32 / total as f32 * 100.
            ),
            Some(total) => format!(
                "{} / {}",
                format_bytes(item.transferred),
                format_bytes(total)
            ),
            None => format_bytes(item.transferred),
        }
    }

    fn render_action_button(button: &ViewHandle<ActionButton>) -> Box<dyn Element> {
        ChildView::new(button).finish()
    }

    fn render_editor(
        &self,
        editor: ViewHandle<EditorView>,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        TextInput::new(
            editor,
            UiComponentStyles::default()
                .set_background(Fill::None)
                .set_border_color(appearance.theme().outline().into())
                .set_border_radius(warpui::elements::CornerRadius::with_all(
                    warpui::elements::Radius::Pixels(4.),
                ))
                .set_padding(Coords::uniform(6.))
                .set_font_size(appearance.ui_font_size()),
        )
        .build()
        .finish()
    }

    fn render_pending_creation_row(
        &self,
        pending: &PendingCreation,
        appearance: &Appearance,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let icon = match pending.kind {
            PendingCreationKind::Directory => Icon::Folder,
            PendingCreationKind::File => Icon::File,
        }
        .to_warpui_icon(theme.accent())
        .finish();
        let details = match pending.kind {
            PendingCreationKind::Directory => "New folder",
            PendingCreationKind::File => "New file",
        };
        Container::new(
            Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(8.)
                .with_child(
                    ConstrainedBox::new(icon)
                        .with_width(16.)
                        .with_height(16.)
                        .finish(),
                )
                .with_child(
                    Shrinkable::new(
                        1.,
                        self.render_editor(self.rename_editor.clone(), appearance),
                    )
                    .finish(),
                )
                .with_child(
                    Text::new_inline(
                        details,
                        appearance.ui_font_family(),
                        appearance.ui_font_size() - 1.,
                    )
                    .with_color(theme.sub_text_color(theme.background()).into())
                    .finish(),
                )
                .finish(),
        )
        .with_horizontal_padding(10.)
        .with_vertical_padding(6.)
        .with_background(internal_colors::accent_overlay_2(theme))
        .finish()
    }

    fn render_directory(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let mut list = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_main_axis_size(MainAxisSize::Max);

        if self.entries.is_empty()
            && self.pending_creation.is_none()
            && !self.is_loading
            && self.error.is_none()
        {
            let position_id = self.position_id.clone();
            list.add_child(
                Hoverable::new(self.empty_directory_mouse_state.clone(), |_| {
                    Container::new(
                        Text::new_inline("Directory is empty", appearance.ui_font_family(), 13.)
                            .with_color(theme.sub_text_color(theme.background()).into())
                            .finish(),
                    )
                    .with_uniform_padding(16.)
                    .finish()
                })
                .on_right_click(move |event, _, position| {
                    let Some(parent_bounds) = event.element_position_by_id(&position_id) else {
                        return;
                    };
                    event.dispatch_typed_action(SftpViewAction::OpenDirectoryContextMenu {
                        position: position - parent_bounds.origin(),
                    });
                })
                .finish(),
            );
        }

        if let Some(pending) = self
            .pending_creation
            .as_ref()
            .filter(|pending| pending.after_path.is_none())
        {
            list.add_child(self.render_pending_creation_row(pending, appearance));
        }

        for (index, entry) in self.entries.iter().enumerate() {
            let selected = self.selected_paths.contains(&entry.path);
            let icon_color = if selected {
                theme.accent()
            } else {
                theme.sub_text_color(theme.background())
            };
            let icon = match entry.file_type {
                RemoteFileType::Directory => Icon::Folder.to_warpui_icon(icon_color).finish(),
                RemoteFileType::File => crate::code::icon_from_file_path(&entry.name, appearance)
                    .unwrap_or_else(|| Icon::File.to_warpui_icon(icon_color).finish()),
                RemoteFileType::Symlink => Icon::Link.to_warpui_icon(icon_color).finish(),
                RemoteFileType::Other => Icon::File.to_warpui_icon(icon_color).finish(),
            };
            let name = if self.pending_rename.as_deref() == Some(entry.path.as_str()) {
                self.render_editor(self.rename_editor.clone(), appearance)
            } else {
                Text::new_inline(
                    entry.name.clone(),
                    appearance.ui_font_family(),
                    appearance.ui_font_size(),
                )
                .with_color(theme.main_text_color(theme.background()).into())
                .finish()
            };
            let details = Text::new_inline(
                if entry.file_type == RemoteFileType::Directory {
                    format!("Folder  {}", format_modified(entry.modified))
                } else {
                    format!(
                        "{}  {}",
                        entry
                            .size
                            .map(format_bytes)
                            .unwrap_or_else(|| "-".to_string()),
                        format_modified(entry.modified)
                    )
                },
                appearance.ui_font_family(),
                appearance.ui_font_size() - 1.,
            )
            .with_color(theme.sub_text_color(theme.background()).into())
            .finish();
            let row = Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(8.)
                .with_child(
                    ConstrainedBox::new(icon)
                        .with_width(16.)
                        .with_height(16.)
                        .finish(),
                )
                .with_child(Shrinkable::new(1., name).finish())
                .with_child(details)
                .finish();
            let mouse_state = self
                .row_mouse_states
                .get(index)
                .cloned()
                .unwrap_or_default();
            let position_id = self.position_id.clone();
            let row = Hoverable::new(mouse_state, move |_| {
                let mut container = Container::new(row)
                    .with_horizontal_padding(10.)
                    .with_vertical_padding(6.);
                if selected {
                    container = container.with_background(internal_colors::accent_overlay_2(theme));
                }
                container.finish()
            })
            .with_cursor(Cursor::PointingHand)
            .on_click_with_modifiers(move |ctx, _, _, modifiers| {
                ctx.dispatch_typed_action(SftpViewAction::SelectEntry { index, modifiers });
            })
            .on_right_click(move |event, _, position| {
                let Some(parent_bounds) = event.element_position_by_id(&position_id) else {
                    return;
                };
                event.dispatch_typed_action(SftpViewAction::OpenContextMenu {
                    position: position - parent_bounds.origin(),
                    index,
                });
            })
            .finish();
            list.add_child(row);
            if let Some(pending) = self
                .pending_creation
                .as_ref()
                .filter(|pending| pending.after_path.as_deref() == Some(entry.path.as_str()))
            {
                list.add_child(self.render_pending_creation_row(pending, appearance));
            }
        }

        let scrollable = ClippedScrollable::vertical(
            self.directory_scroll_state.clone(),
            list.finish(),
            ScrollbarWidth::Auto,
            theme.nonactive_ui_detail().into(),
            theme.active_ui_detail().into(),
            Fill::None,
        )
        .with_overlayed_scrollbar()
        .finish();
        ConstrainedBox::new(scrollable)
            .with_min_height(160.)
            .finish()
    }

    fn render_transfers(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let mut list = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_spacing(4.);
        for item in &self.transfers {
            let status = match item.status {
                TransferStatus::Running => Self::transfer_progress(item),
                TransferStatus::Completed => "Completed".to_string(),
                TransferStatus::Cancelled => "Cancelled".to_string(),
                TransferStatus::AwaitingReplacement => item
                    .error
                    .clone()
                    .unwrap_or_else(|| "Replace existing file?".to_string()),
                TransferStatus::Failed => item
                    .error
                    .clone()
                    .unwrap_or_else(|| "Transfer failed".to_string()),
            };
            let mut actions = Flex::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(4.);
            match item.status {
                TransferStatus::Running => {
                    actions.add_child(Self::render_action_button(&item.cancel_button))
                }
                TransferStatus::Failed | TransferStatus::Cancelled => {
                    actions.add_child(Self::render_action_button(&item.retry_button));
                }
                TransferStatus::AwaitingReplacement => {
                    actions.add_child(Self::render_action_button(&item.replace_button));
                    actions.add_child(Self::render_action_button(&item.cancel_button));
                }
                TransferStatus::Completed => {}
            }
            list.add_child(
                Flex::row()
                    .with_main_axis_size(MainAxisSize::Max)
                    .with_cross_axis_alignment(CrossAxisAlignment::Center)
                    .with_spacing(8.)
                    .with_child(
                        Shrinkable::new(
                            1.,
                            Text::new_inline(
                                format!("{}  -  {status}", item.name),
                                appearance.ui_font_family(),
                                12.,
                            )
                            .with_color(theme.main_text_color(theme.background()).into())
                            .finish(),
                        )
                        .finish(),
                    )
                    .with_child(actions.finish())
                    .finish(),
            );
        }
        let scrollable = ClippedScrollable::vertical(
            self.transfer_scroll_state.clone(),
            list.finish(),
            ScrollbarWidth::Auto,
            theme.nonactive_ui_detail().into(),
            theme.active_ui_detail().into(),
            Fill::None,
        )
        .with_overlayed_scrollbar()
        .finish();
        ConstrainedBox::new(scrollable)
            .with_max_height(140.)
            .finish()
    }

    fn render_content(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let mut root = Flex::column()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_spacing(8.);

        let target_input = self.render_editor(self.target_editor.clone(), appearance);
        root.add_child(
            Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(8.)
                .with_child(
                    Text::new_inline("Target", appearance.ui_font_family(), 12.)
                        .with_color(theme.sub_text_color(theme.background()).into())
                        .finish(),
                )
                .with_child(Shrinkable::new(1., target_input).finish())
                .with_child(if self.is_connecting {
                    Self::render_action_button(&self.cancel_connect_button)
                } else {
                    Self::render_action_button(&self.connect_button)
                })
                .finish(),
        );

        if !self.aliases.is_empty() {
            let mut aliases = Wrap::row()
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .with_spacing(4.)
                .with_run_spacing(4.);
            aliases.add_child(
                Text::new_inline("SSH aliases", appearance.ui_font_family(), 11.)
                    .with_color(theme.sub_text_color(theme.background()).into())
                    .finish(),
            );
            for alias_button in &self.aliases {
                aliases.add_child(Self::render_action_button(alias_button));
            }
            root.add_child(aliases.finish());
        }

        let path_input = self.render_editor(self.path_editor.clone(), appearance);
        let mut navigation = Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_spacing(6.)
            .with_child(Shrinkable::new(1., path_input).finish());
        navigation.add_child(Self::render_action_button(&self.navigate_up_button));
        navigation.add_child(Self::render_action_button(&self.refresh_button));
        root.add_child(navigation.finish());

        if let Some(error) = &self.error {
            root.add_child(
                Text::new_inline(error.clone(), appearance.ui_font_family(), 12.)
                    .with_color(theme.ui_error_color().into())
                    .soft_wrap(true)
                    .finish(),
            );
        } else if self.is_connecting {
            root.add_child(
                Text::new_inline("Connecting...", appearance.ui_font_family(), 12.)
                    .with_color(theme.sub_text_color(theme.background()).into())
                    .finish(),
            );
        } else if self.is_loading {
            root.add_child(
                Text::new_inline("Loading...", appearance.ui_font_family(), 12.)
                    .with_color(theme.sub_text_color(theme.background()).into())
                    .finish(),
            );
        } else if self.is_opening_file {
            root.add_child(
                Text::new_inline("Opening file...", appearance.ui_font_family(), 12.)
                    .with_color(theme.sub_text_color(theme.background()).into())
                    .finish(),
            );
        }

        let toolbar = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_spacing(6.)
            .with_child(Self::render_action_button(&self.upload_button))
            .with_child(Self::render_action_button(&self.download_button))
            .with_child(
                Shrinkable::new(
                    1.,
                    Container::new(
                        Text::new_inline(
                            format!("{} selected", self.selected_paths.len()),
                            appearance.ui_font_family(),
                            11.,
                        )
                        .with_color(theme.sub_text_color(theme.background()).into())
                        .finish(),
                    )
                    .finish(),
                )
                .finish(),
            )
            .with_child(
                if self
                    .transfers
                    .iter()
                    .any(|item| item.status == TransferStatus::Running)
                {
                    Self::render_action_button(&self.cancel_all_button)
                } else {
                    Container::new(Text::new_inline("", appearance.ui_font_family(), 1.).finish())
                        .finish()
                },
            )
            .finish();
        root.add_child(toolbar);

        root.add_child(Shrinkable::new(1., self.render_directory(app)).finish());
        if !self.transfers.is_empty() {
            root.add_child(
                Container::new(
                    Flex::column()
                        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
                        .with_child(
                            Text::new_inline("Transfers", appearance.ui_font_family(), 12.)
                                .with_color(theme.sub_text_color(theme.background()).into())
                                .finish(),
                        )
                        .with_child(self.render_transfers(app))
                        .finish(),
                )
                .with_background(theme.surface_1())
                .with_padding_left(8.)
                .with_padding_right(8.)
                .finish(),
            );
        }

        let content = Container::new(root.finish())
            .with_padding_left(12.)
            .with_padding_right(12.)
            .with_padding_top(10.)
            .with_padding_bottom(10.)
            .finish();
        let mut stack = Stack::new();
        stack.add_child(SavePosition::new(content, &self.position_id).finish());
        if let Some(context_menu_state) = &self.context_menu_state {
            stack.add_positioned_overlay_child(
                ChildView::new(&self.context_menu).finish(),
                OffsetPositioning::offset_from_parent(
                    context_menu_state.position,
                    ParentOffsetBounds::WindowByPosition,
                    ParentAnchor::TopLeft,
                    ChildAnchor::TopLeft,
                ),
            );
        }
        stack.finish()
    }
}

impl Entity for SftpView {
    type Event = SftpViewEvent;
}

impl View for SftpView {
    fn ui_name() -> &'static str {
        "SftpView"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.render_content(app)
    }
}

impl TypedActionView for SftpView {
    type Action = SftpViewAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            SftpViewAction::Connect => self.connect_from_editor(ctx),
            SftpViewAction::CancelConnect => self.cancel_connect(ctx),
            SftpViewAction::Refresh => self.load_directory(self.remote_path.clone(), ctx),
            SftpViewAction::NavigateUp => {
                self.load_directory(parent_remote_path(&self.remote_path), ctx)
            }
            SftpViewAction::SelectAlias(alias) => {
                self.target_editor.update(ctx, |editor, ctx| {
                    editor.set_buffer_text(alias, ctx);
                });
                self.connect(alias.clone(), None, ctx);
            }
            SftpViewAction::SelectEntry { index, modifiers } => {
                self.select_entry(*index, *modifiers, ctx)
            }
            SftpViewAction::OpenContextMenu { position, index } => {
                self.open_context_menu(*position, *index, ctx)
            }
            SftpViewAction::OpenDirectoryContextMenu { position } => {
                self.open_directory_context_menu(*position, ctx)
            }
            SftpViewAction::OpenEntry(path) => self.open_entry(path.clone(), ctx),
            SftpViewAction::EditEntry(path) => self.edit_entry(path.clone(), ctx),
            SftpViewAction::RenameEntry(path) => self.start_rename(path.clone(), ctx),
            SftpViewAction::StartCreateDirectory { parent, after_path } => self.start_create(
                parent.clone(),
                after_path.clone(),
                PendingCreationKind::Directory,
                ctx,
            ),
            SftpViewAction::StartCreateFile { parent, after_path } => self.start_create(
                parent.clone(),
                after_path.clone(),
                PendingCreationKind::File,
                ctx,
            ),
            SftpViewAction::DeleteEntry(path) => self.delete_entry(path.clone(), ctx),
            SftpViewAction::DownloadEntry(path) => {
                self.context_menu_state = None;
                if !self.selected_paths.contains(path) {
                    self.selected_paths.clear();
                    self.selected_paths.insert(path.clone());
                    self.selection_anchor =
                        self.entries.iter().position(|entry| entry.path == *path);
                }
                self.open_download_picker(ctx);
            }
            SftpViewAction::OpenUploadPicker => {
                if self.connection.is_some() {
                    self.open_upload_picker(ctx);
                } else {
                    self.error = Some("Connect to a host first".to_string());
                    ctx.notify();
                }
            }
            SftpViewAction::OpenDownloadPicker => {
                if self.connection.is_none() {
                    self.error = Some("Connect to a host first".to_string());
                    ctx.notify();
                } else if self.selected_paths.is_empty() {
                    self.error = Some("Select one or more remote files first".to_string());
                    ctx.notify();
                } else {
                    self.open_download_picker(ctx);
                }
            }
            SftpViewAction::UploadPaths(paths) => self.enqueue_uploads(paths.clone(), ctx),
            SftpViewAction::DownloadDirectory(path) => self.enqueue_download(path.clone(), ctx),
            SftpViewAction::FilePickerError(error) => {
                self.error = Some(error.clone());
                ctx.notify();
            }
            SftpViewAction::CancelTransfer(id) => self.cancel_transfer(*id, ctx),
            SftpViewAction::RetryTransfer(id) => self.run_transfer(*id, false, ctx),
            SftpViewAction::ReplaceTransfer(id) => self.run_transfer(*id, true, ctx),
            SftpViewAction::CancelAllTransfers => {
                self.cancel_all_transfers();
                ctx.notify();
            }
        }
    }
}

impl BackingView for SftpView {
    type PaneHeaderOverflowMenuAction = SftpViewAction;
    type CustomAction = SftpViewAction;
    type AssociatedData = ();

    fn handle_pane_header_overflow_menu_action(
        &mut self,
        action: &Self::PaneHeaderOverflowMenuAction,
        ctx: &mut ViewContext<Self>,
    ) {
        self.handle_action(action, ctx);
    }

    fn handle_custom_action(&mut self, action: &Self::CustomAction, ctx: &mut ViewContext<Self>) {
        self.handle_action(action, ctx);
    }

    fn close(&mut self, ctx: &mut ViewContext<Self>) {
        self.shutdown();
        ctx.emit(SftpViewEvent::Pane(PaneEvent::Close));
    }

    fn focus_contents(&mut self, ctx: &mut ViewContext<Self>) {
        self.focus(ctx);
    }

    fn render_header_content(
        &self,
        _ctx: &view::HeaderRenderContext<'_>,
        _app: &AppContext,
    ) -> HeaderContent {
        HeaderContent::Standard(StandardHeader {
            title: SFTP_HEADER_TEXT.to_string(),
            title_secondary: (!self.target.is_empty()).then(|| self.target.clone()),
            title_style: None,
            title_clip_config: warpui::text_layout::ClipConfig::start(),
            title_max_width: None,
            left_of_title: None,
            right_of_title: None,
            left_of_overflow: None,
            options: StandardHeaderOptions {
                always_show_icons: true,
                ..StandardHeaderOptions::default()
            },
        })
    }

    fn set_focus_handle(&mut self, focus_handle: PaneFocusHandle, _ctx: &mut ViewContext<Self>) {
        self.focus_handle = Some(focus_handle);
    }
}

fn update_selection(
    paths: &[String],
    selected_paths: &mut BTreeSet<String>,
    selection_anchor: &mut Option<usize>,
    index: usize,
    modifiers: SelectionModifiers,
) {
    let Some(path) = paths.get(index) else {
        return;
    };

    if modifiers.range {
        let anchor = selection_anchor.unwrap_or(index).min(paths.len() - 1);
        if !modifiers.additive {
            selected_paths.clear();
        }
        let (start, end) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        selected_paths.extend(paths[start..=end].iter().cloned());
        if selection_anchor.is_none() {
            *selection_anchor = Some(index);
        }
        return;
    }

    *selection_anchor = Some(index);
    if modifiers.additive {
        if !selected_paths.remove(path) {
            selected_paths.insert(path.clone());
        }
    } else {
        selected_paths.clear();
        selected_paths.insert(path.clone());
    }
}

fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_modified(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return "-".to_string();
    };
    let datetime: DateTime<Local> = modified.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{format_bytes, update_selection, SelectionModifiers};

    #[test]
    fn formats_transfer_sizes_for_compact_rows() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn selects_single_entries_and_toggles_additive_entries() {
        let paths = ["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut selected = BTreeSet::new();
        let mut anchor = None;

        update_selection(
            &paths,
            &mut selected,
            &mut anchor,
            1,
            SelectionModifiers::default(),
        );
        assert_eq!(selected, BTreeSet::from(["b".to_string()]));
        assert_eq!(anchor, Some(1));

        let additive = SelectionModifiers {
            additive: true,
            range: false,
        };
        update_selection(&paths, &mut selected, &mut anchor, 3, additive);
        assert_eq!(selected, BTreeSet::from(["b".to_string(), "d".to_string()]));
        update_selection(&paths, &mut selected, &mut anchor, 3, additive);
        assert_eq!(selected, BTreeSet::from(["b".to_string()]));
    }

    #[test]
    fn selects_ranges_and_adds_ranges_to_existing_selection() {
        let paths = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut selected = BTreeSet::from(["a".to_string()]);
        let mut anchor = Some(1);

        update_selection(
            &paths,
            &mut selected,
            &mut anchor,
            4,
            SelectionModifiers {
                additive: false,
                range: true,
            },
        );
        assert_eq!(
            selected,
            BTreeSet::from([
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
            ])
        );
        assert_eq!(anchor, Some(1));

        update_selection(
            &paths,
            &mut selected,
            &mut anchor,
            5,
            SelectionModifiers {
                additive: true,
                range: true,
            },
        );
        assert_eq!(
            selected,
            BTreeSet::from([
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f".to_string(),
            ])
        );
    }
}
