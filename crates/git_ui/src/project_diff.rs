use crate::git_panel::{GitPanel, GitPanelAddon, GitStatusEntry};
use anyhow::Result;
use buffer_diff::{BufferDiff, DiffHunkSecondaryStatus};
use collections::HashSet;
use editor::{
    actions::{GoToHunk, GoToPreviousHunk},
    scroll::Autoscroll,
    Editor, EditorEvent,
};
use feature_flags::FeatureFlagViewExt;
use futures::StreamExt;
use git::{
    status::FileStatus, ShowCommitEditor, StageAll, StageAndNext, ToggleStaged, UnstageAll,
    UnstageAndNext,
};
use gpui::{
    actions, Action, AnyElement, AnyView, App, AppContext as _, AsyncWindowContext, Entity,
    EventEmitter, FocusHandle, Focusable, Render, Subscription, Task, WeakEntity,
};
use language::{Anchor, Buffer, Capability, OffsetRangeExt};
use multi_buffer::{MultiBuffer, PathKey};
use project::{git::GitStore, Project, ProjectPath};
use std::any::{Any, TypeId};
use theme::ActiveTheme;
use ui::{prelude::*, vertical_divider, Tooltip};
use util::ResultExt as _;
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle, TabContentParams},
    searchable::SearchableItemHandle,
    ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
};

actions!(git, [Diff]);

pub struct ProjectDiff {
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    project: Entity<Project>,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    update_needed: postage::watch::Sender<()>,
    pending_scroll: Option<PathKey>,

    _task: Task<Result<()>>,
    _subscription: Subscription,
}

#[derive(Debug)]
struct DiffBuffer {
    path_key: PathKey,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    file_status: FileStatus,
}

const CONFLICT_NAMESPACE: &'static str = "0";
const TRACKED_NAMESPACE: &'static str = "1";
const NEW_NAMESPACE: &'static str = "2";

impl ProjectDiff {
    pub(crate) fn register(
        _: &mut Workspace,
        window: Option<&mut Window>,
        cx: &mut Context<Workspace>,
    ) {
        let Some(window) = window else { return };
        cx.when_flag_enabled::<feature_flags::GitUiFeatureFlag>(window, |workspace, _, _cx| {
            workspace.register_action(Self::deploy);
        });

        workspace::register_serializable_item::<ProjectDiff>(cx);
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &Diff,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::deploy_at(workspace, None, window, cx)
    }

    pub fn deploy_at(
        workspace: &mut Workspace,
        entry: Option<GitStatusEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project_diff = if let Some(existing) = workspace.item_of_type::<Self>(cx) {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity();
            let project_diff =
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(
                Box::new(project_diff.clone()),
                None,
                true,
                window,
                cx,
            );
            project_diff
        };
        if let Some(entry) = entry {
            project_diff.update(cx, |project_diff, cx| {
                project_diff.move_to_entry(entry, window, cx);
            })
        }
    }

    fn new(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));

        let editor = cx.new(|cx| {
            let mut diff_display_editor = Editor::for_multibuffer(
                multibuffer.clone(),
                Some(project.clone()),
                true,
                window,
                cx,
            );
            diff_display_editor.set_expand_all_diff_hunks(cx);
            diff_display_editor.register_addon(GitPanelAddon {
                workspace: workspace.downgrade(),
            });
            diff_display_editor
        });
        cx.subscribe_in(&editor, window, Self::handle_editor_event)
            .detach();

        let git_store = project.read(cx).git_store().clone();
        let git_store_subscription = cx.subscribe_in(
            &git_store,
            window,
            move |this, _git_store, _event, _window, _cx| {
                *this.update_needed.borrow_mut() = ();
            },
        );

        let (mut send, recv) = postage::watch::channel::<()>();
        let worker = window.spawn(cx, {
            let this = cx.weak_entity();
            |cx| Self::handle_status_updates(this, recv, cx)
        });
        // Kick of a refresh immediately
        *send.borrow_mut() = ();

        Self {
            project,
            git_store: git_store.clone(),
            workspace: workspace.downgrade(),
            focus_handle,
            editor,
            multibuffer,
            pending_scroll: None,
            update_needed: send,
            _task: worker,
            _subscription: git_store_subscription,
        }
    }

    pub fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(git_repo) = self.git_store.read(cx).active_repository() else {
            return;
        };
        let repo = git_repo.read(cx);

        let namespace = if repo.has_conflict(&entry.repo_path) {
            CONFLICT_NAMESPACE
        } else if entry.status.is_created() {
            NEW_NAMESPACE
        } else {
            TRACKED_NAMESPACE
        };

        let path_key = PathKey::namespaced(namespace, entry.repo_path.0.clone());

        self.move_to_path(path_key, window, cx)
    }

    pub fn active_path(&self, cx: &App) -> Option<ProjectPath> {
        let editor = self.editor.read(cx);
        let position = editor.selections.newest_anchor().head();
        let multi_buffer = editor.buffer().read(cx);
        let (_, buffer, _) = multi_buffer.excerpt_containing(position, cx)?;

        let file = buffer.read(cx).file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    fn move_to_path(&mut self, path_key: PathKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.multibuffer.read(cx).location_for_path(&path_key, cx) {
            self.editor.update(cx, |editor, cx| {
                editor.change_selections(Some(Autoscroll::focused()), window, cx, |s| {
                    s.select_ranges([position..position]);
                })
            });
        } else {
            self.pending_scroll = Some(path_key);
        }
    }

    fn button_states(&self, cx: &App) -> ButtonStates {
        let editor = self.editor.read(cx);
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let prev_next = snapshot.diff_hunks().skip(1).next().is_some();
        let mut selection = true;

        let mut ranges = editor
            .selections
            .disjoint_anchor_ranges()
            .collect::<Vec<_>>();
        if !ranges.iter().any(|range| range.start != range.end) {
            selection = false;
            if let Some((excerpt_id, buffer, range)) = self.editor.read(cx).active_excerpt(cx) {
                ranges = vec![multi_buffer::Anchor::range_in_buffer(
                    excerpt_id,
                    buffer.read(cx).remote_id(),
                    range,
                )];
            } else {
                ranges = Vec::default();
            }
        }
        let mut has_staged_hunks = false;
        let mut has_unstaged_hunks = false;
        for hunk in editor.diff_hunks_in_ranges(&ranges, &snapshot) {
            match hunk.secondary_status {
                DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending => {
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk => {
                    has_staged_hunks = true;
                    has_unstaged_hunks = true;
                }
                DiffHunkSecondaryStatus::None
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending => {
                    has_staged_hunks = true;
                }
            }
        }
        let mut stage_all = false;
        let mut unstage_all = false;
        self.workspace
            .read_with(cx, |workspace, cx| {
                if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                    let git_panel = git_panel.read(cx);
                    stage_all = git_panel.can_stage_all();
                    unstage_all = git_panel.can_unstage_all();
                }
            })
            .ok();

        return ButtonStates {
            stage: has_unstaged_hunks,
            unstage: has_staged_hunks,
            prev_next,
            selection,
            stage_all,
            unstage_all,
        };
    }

    fn handle_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::SelectionsChanged { local: true } => {
                let Some(project_path) = self.active_path(cx) else {
                    return;
                };
                self.workspace
                    .update(cx, |workspace, cx| {
                        if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
                            git_panel.update(cx, |git_panel, cx| {
                                git_panel.select_entry_by_path(project_path, window, cx)
                            })
                        }
                    })
                    .ok();
            }
            _ => {}
        }
    }

    fn load_buffers(&mut self, cx: &mut Context<Self>) -> Vec<Task<Result<DiffBuffer>>> {
        let Some(repo) = self.git_store.read(cx).active_repository() else {
            self.multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.clear(cx);
            });
            return vec![];
        };

        let mut previous_paths = self.multibuffer.read(cx).paths().collect::<HashSet<_>>();

        let mut result = vec![];
        repo.update(cx, |repo, cx| {
            for entry in repo.status() {
                if !entry.status.has_changes() {
                    continue;
                }
                let Some(project_path) = repo.repo_path_to_project_path(&entry.repo_path) else {
                    continue;
                };
                let namespace = if repo.has_conflict(&entry.repo_path) {
                    CONFLICT_NAMESPACE
                } else if entry.status.is_created() {
                    NEW_NAMESPACE
                } else {
                    TRACKED_NAMESPACE
                };
                let path_key = PathKey::namespaced(namespace, entry.repo_path.0.clone());

                previous_paths.remove(&path_key);
                let load_buffer = self
                    .project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx));

                let project = self.project.clone();
                result.push(cx.spawn(|_, mut cx| async move {
                    let buffer = load_buffer.await?;
                    let changes = project
                        .update(&mut cx, |project, cx| {
                            project.open_uncommitted_diff(buffer.clone(), cx)
                        })?
                        .await?;
                    Ok(DiffBuffer {
                        path_key,
                        buffer,
                        diff: changes,
                        file_status: entry.status,
                    })
                }));
            }
        });
        self.multibuffer.update(cx, |multibuffer, cx| {
            for path in previous_paths {
                multibuffer.remove_excerpts_for_path(path, cx);
            }
        });
        result
    }

    fn register_buffer(
        &mut self,
        diff_buffer: DiffBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_key = diff_buffer.path_key;
        let buffer = diff_buffer.buffer;
        let diff = diff_buffer.diff;

        let snapshot = buffer.read(cx).snapshot();
        let diff = diff.read(cx);
        let diff_hunk_ranges = diff
            .hunks_intersecting_range(Anchor::MIN..Anchor::MAX, &snapshot, cx)
            .map(|diff_hunk| diff_hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>();

        let (was_empty, is_excerpt_newly_added) = self.multibuffer.update(cx, |multibuffer, cx| {
            let was_empty = multibuffer.is_empty();
            let is_newly_added = multibuffer.set_excerpts_for_path(
                path_key.clone(),
                buffer,
                diff_hunk_ranges,
                editor::DEFAULT_MULTIBUFFER_CONTEXT,
                cx,
            );
            (was_empty, is_newly_added)
        });

        self.editor.update(cx, |editor, cx| {
            if was_empty {
                editor.change_selections(None, window, cx, |selections| {
                    // TODO select the very beginning (possibly inside a deletion)
                    selections.select_ranges([0..0])
                });
            }
            if is_excerpt_newly_added && diff_buffer.file_status.is_deleted() {
                editor.fold_buffer(snapshot.text.remote_id(), cx)
            }
        });

        if self.multibuffer.read(cx).is_empty()
            && self
                .editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.focus_handle.focus(window);
        } else if self.focus_handle.is_focused(window) && !self.multibuffer.read(cx).is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window);
            });
        }
        if self.pending_scroll.as_ref() == Some(&path_key) {
            self.move_to_path(path_key, window, cx);
        }
    }

    pub async fn handle_status_updates(
        this: WeakEntity<Self>,
        mut recv: postage::watch::Receiver<()>,
        mut cx: AsyncWindowContext,
    ) -> Result<()> {
        while let Some(_) = recv.next().await {
            let buffers_to_load = this.update(&mut cx, |this, cx| this.load_buffers(cx))?;
            for buffer_to_load in buffers_to_load {
                if let Some(buffer) = buffer_to_load.await.log_err() {
                    cx.update(|window, cx| {
                        this.update(cx, |this, cx| this.register_buffer(buffer, window, cx))
                            .ok();
                    })?;
                }
            }
            this.update(&mut cx, |this, _| this.pending_scroll.take())?;
        }

        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn excerpt_paths(&self, cx: &App) -> Vec<String> {
        self.multibuffer
            .read(cx)
            .excerpt_paths()
            .map(|key| key.path().to_string_lossy().to_string())
            .collect()
    }
}

impl EventEmitter<EditorEvent> for ProjectDiff {}

impl Focusable for ProjectDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for ProjectDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Box<dyn Any>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Project Diff".into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _: &App) -> AnyElement {
        Label::new("Uncommitted Changes")
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Project Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &App) -> bool {
        false
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Self>>
    where
        Self: Sized,
    {
        let workspace = self.workspace.upgrade()?;
        Some(cx.new(|cx| ProjectDiff::new(self.project.clone(), workspace, window, cx)))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(format, project, window, cx)
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.reload(project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ProjectDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();

        div()
            .track_focus(&self.focus_handle)
            .key_context(if is_empty { "EmptyPane" } else { "GitDiff" })
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .when(is_empty, |el| {
                el.child(Label::new("No uncommitted changes"))
            })
            .when(!is_empty, |el| el.child(self.editor.clone()))
    }
}

impl SerializableItem for ProjectDiff {
    fn serialized_item_kind() -> &'static str {
        "ProjectDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, |mut cx| async move {
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                cx.new(|cx| Self::new(workspace.project().clone(), workspace_handle, window, cx))
            })
        })
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        _item_id: workspace::ItemId,
        _closing: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        None
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub struct ProjectDiffToolbar {
    project_diff: Option<WeakEntity<ProjectDiff>>,
    workspace: WeakEntity<Workspace>,
}

impl ProjectDiffToolbar {
    pub fn new(workspace: &Workspace, _: &mut Context<Self>) -> Self {
        Self {
            project_diff: None,
            workspace: workspace.weak_handle(),
        }
    }

    fn project_diff(&self, _: &App) -> Option<Entity<ProjectDiff>> {
        self.project_diff.as_ref()?.upgrade()
    }
    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project_diff) = self.project_diff(cx) {
            project_diff.focus_handle(cx).focus(window);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }
    fn dispatch_panel_action(
        &self,
        action: &dyn Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .read_with(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<GitPanel>(cx) {
                    panel.focus_handle(cx).focus(window)
                }
            })
            .ok();
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for ProjectDiffToolbar {}

impl ToolbarItemView for ProjectDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.project_diff = active_pane_item
            .and_then(|item| item.act_as::<ProjectDiff>(cx))
            .map(|entity| entity.downgrade());
        if self.project_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

struct ButtonStates {
    stage: bool,
    unstage: bool,
    prev_next: bool,
    selection: bool,
    stage_all: bool,
    unstage_all: bool,
}

impl Render for ProjectDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project_diff) = self.project_diff(cx) else {
            return div();
        };
        let focus_handle = project_diff.focus_handle(cx);
        let button_states = project_diff.read(cx).button_states(cx);

        h_group_xl()
            .my_neg_1()
            .items_center()
            .py_1()
            .pl_2()
            .pr_1()
            .flex_wrap()
            .justify_between()
            .child(
                h_group_sm()
                    .when(button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Toggle Staged")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Toggle Staged",
                                    &ToggleStaged,
                                    &focus_handle,
                                ))
                                .disabled(!button_states.stage && !button_states.unstage)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&ToggleStaged, window, cx)
                                })),
                        )
                    })
                    .when(!button_states.selection, |el| {
                        el.child(
                            Button::new("stage", "Stage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Stage and go to next hunk",
                                    &StageAndNext,
                                    &focus_handle,
                                ))
                                // don't actually disable the button so it's mashable
                                .color(if button_states.stage {
                                    Color::Default
                                } else {
                                    Color::Disabled
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&StageAndNext, window, cx)
                                })),
                        )
                        .child(
                            Button::new("unstage", "Unstage")
                                .tooltip(Tooltip::for_action_title_in(
                                    "Unstage and go to next hunk",
                                    &UnstageAndNext,
                                    &focus_handle,
                                ))
                                .color(if button_states.unstage {
                                    Color::Default
                                } else {
                                    Color::Disabled
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dispatch_action(&UnstageAndNext, window, cx)
                                })),
                        )
                    }),
            )
            // n.b. the only reason these arrows are here is because we don't
            // support "undo" for staging so we need a way to go back.
            .child(
                h_group_sm()
                    .child(
                        IconButton::new("up", IconName::ArrowUp)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to previous hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToPreviousHunk, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("down", IconName::ArrowDown)
                            .shape(ui::IconButtonShape::Square)
                            .tooltip(Tooltip::for_action_title_in(
                                "Go to next hunk",
                                &GoToHunk,
                                &focus_handle,
                            ))
                            .disabled(!button_states.prev_next)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&GoToHunk, window, cx)
                            })),
                    ),
            )
            .child(vertical_divider())
            .child(
                h_group_sm()
                    .when(
                        button_states.unstage_all && !button_states.stage_all,
                        |el| {
                            el.child(
                                Button::new("unstage-all", "Unstage All")
                                    .tooltip(Tooltip::for_action_title_in(
                                        "Unstage all changes",
                                        &UnstageAll,
                                        &focus_handle,
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dispatch_panel_action(&UnstageAll, window, cx)
                                    })),
                            )
                        },
                    )
                    .when(
                        !button_states.unstage_all || button_states.stage_all,
                        |el| {
                            el.child(
                                // todo make it so that changing to say "Unstaged"
                                // doesn't change the position.
                                div().child(
                                    Button::new("stage-all", "Stage All")
                                        .disabled(!button_states.stage_all)
                                        .tooltip(Tooltip::for_action_title_in(
                                            "Stage all changes",
                                            &StageAll,
                                            &focus_handle,
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.dispatch_panel_action(&StageAll, window, cx)
                                        })),
                                ),
                            )
                        },
                    )
                    .child(
                        Button::new("commit", "Commit")
                            .tooltip(Tooltip::for_action_title_in(
                                "Commit",
                                &ShowCommitEditor,
                                &focus_handle,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&ShowCommitEditor, window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use collections::HashMap;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::status::{StatusCode, TrackedStatus};
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use unindent::Unindent as _;
    use util::path;

    use super::*;

    #[ctor::ctor]
    fn init_logger() {
        env_logger::init();
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme::init(theme::LoadThemes::JustBase, cx);
            language::init(cx);
            Project::init_settings(cx);
            workspace::init_settings(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test]
    async fn test_save_after_restore(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo.txt": "FOO\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo.txt".into(), "foo\n".into())],
        );
        fs.set_index_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo.txt".into(), "foo\n".into())],
        );
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.statuses = HashMap::from_iter([(
                "foo.txt".into(),
                TrackedStatus {
                    index_status: StatusCode::Unmodified,
                    worktree_status: StatusCode::Modified,
                }
                .into(),
            )]);
        });
        cx.run_until_parked();

        let editor = diff.update(cx, |diff, _| diff.editor.clone());
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - foo
                + ˇFOO
            "
            .unindent(),
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.git_restore(&Default::default(), window, cx);
        });
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.statuses = HashMap::default();
        });
        cx.run_until_parked();

        assert_state_with_diff(&editor, cx, &"ˇ".unindent());

        let text = String::from_utf8(fs.read_file_sync("/project/foo.txt").unwrap()).unwrap();
        assert_eq!(text, "foo\n");
    }

    #[gpui::test]
    async fn test_scroll_to_beginning_with_deletion(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "bar": "BAR\n",
                "foo": "FOO\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("bar".into(), "bar\n".into()),
                ("foo".into(), "foo\n".into()),
            ],
        );
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.statuses = HashMap::from_iter([
                (
                    "bar".into(),
                    TrackedStatus {
                        index_status: StatusCode::Unmodified,
                        worktree_status: StatusCode::Modified,
                    }
                    .into(),
                ),
                (
                    "foo".into(),
                    TrackedStatus {
                        index_status: StatusCode::Unmodified,
                        worktree_status: StatusCode::Modified,
                    }
                    .into(),
                ),
            ]);
        });
        cx.run_until_parked();

        let editor = cx.update_window_entity(&diff, |diff, window, cx| {
            diff.move_to_path(
                PathKey::namespaced(TRACKED_NAMESPACE, Path::new("foo").into()),
                window,
                cx,
            );
            diff.editor.clone()
        });
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - bar
                + BAR

                - ˇfoo
                + FOO
            "
            .unindent(),
        );

        let editor = cx.update_window_entity(&diff, |diff, window, cx| {
            diff.move_to_path(
                PathKey::namespaced(TRACKED_NAMESPACE, Path::new("bar").into()),
                window,
                cx,
            );
            diff.editor.clone()
        });
        assert_state_with_diff(
            &editor,
            cx,
            &"
                - ˇbar
                + BAR

                - foo
                + FOO
            "
            .unindent(),
        );
    }

    #[gpui::test]
    async fn test_hunks_after_restore_then_modify(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "foo": "modified\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/foo"), cx)
            })
            .await
            .unwrap();
        let buffer_editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });
        let diff = cx.new_window_entity(|window, cx| {
            ProjectDiff::new(project.clone(), workspace, window, cx)
        });
        cx.run_until_parked();

        fs.set_head_for_repo(
            path!("/project/.git").as_ref(),
            &[("foo".into(), "original\n".into())],
        );
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state.statuses = HashMap::from_iter([(
                "foo".into(),
                TrackedStatus {
                    index_status: StatusCode::Unmodified,
                    worktree_status: StatusCode::Modified,
                }
                .into(),
            )]);
        });
        cx.run_until_parked();

        let diff_editor = diff.update(cx, |diff, _| diff.editor.clone());

        assert_state_with_diff(
            &diff_editor,
            cx,
            &"
                - original
                + ˇmodified
            "
            .unindent(),
        );

        let prev_buffer_hunks =
            cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
                let snapshot = buffer_editor.snapshot(window, cx);
                let snapshot = &snapshot.buffer_snapshot;
                let prev_buffer_hunks = buffer_editor
                    .diff_hunks_in_ranges(&[editor::Anchor::min()..editor::Anchor::max()], snapshot)
                    .collect::<Vec<_>>();
                buffer_editor.git_restore(&Default::default(), window, cx);
                prev_buffer_hunks
            });
        assert_eq!(prev_buffer_hunks.len(), 1);
        cx.run_until_parked();

        let new_buffer_hunks =
            cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
                let snapshot = buffer_editor.snapshot(window, cx);
                let snapshot = &snapshot.buffer_snapshot;
                let new_buffer_hunks = buffer_editor
                    .diff_hunks_in_ranges(&[editor::Anchor::min()..editor::Anchor::max()], snapshot)
                    .collect::<Vec<_>>();
                buffer_editor.git_restore(&Default::default(), window, cx);
                new_buffer_hunks
            });
        assert_eq!(new_buffer_hunks.as_slice(), &[]);

        cx.update_window_entity(&buffer_editor, |buffer_editor, window, cx| {
            buffer_editor.set_text("different\n", window, cx);
            buffer_editor.save(false, project.clone(), window, cx)
        })
        .await
        .unwrap();

        cx.run_until_parked();

        assert_state_with_diff(
            &diff_editor,
            cx,
            &"
                - original
                + ˇdifferent
            "
            .unindent(),
        );
    }
}
