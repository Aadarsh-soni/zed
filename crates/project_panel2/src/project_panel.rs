pub mod file_associations;
mod project_panel_settings;
use settings::{Settings, SettingsStore};

use db::kvp::KEY_VALUE_STORE;
use editor::{scroll::autoscroll::Autoscroll, Cancel, Editor};
use file_associations::FileAssociations;

use anyhow::{anyhow, Result};
use gpui::{
    actions, div, overlay, px, uniform_list, Action, AppContext, AssetSource, AsyncWindowContext,
    ClipboardItem, DismissEvent, Div, EventEmitter, FocusHandle, Focusable, FocusableView,
    InteractiveElement, KeyContext, Model, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, PromptLevel, Render, Stateful, Styled, Subscription, Task, UniformListScrollHandle,
    View, ViewContext, VisualContext as _, WeakView, WindowContext,
};
use menu::{Confirm, SelectNext, SelectPrev};
use project::{
    repository::GitFileStatus, Entry, EntryKind, Fs, Project, ProjectEntryId, ProjectPath,
    Worktree, WorktreeId,
};
use project_panel_settings::{ProjectPanelDockPosition, ProjectPanelSettings};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{hash_map, HashMap},
    ffi::OsStr,
    ops::Range,
    path::Path,
    sync::Arc,
};
use ui::{prelude::*, v_stack, ContextMenu, IconElement, Label, ListItem};
use unicase::UniCase;
use util::{maybe, ResultExt, TryFutureExt};
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    Workspace,
};

const PROJECT_PANEL_KEY: &'static str = "ProjectPanel";
const NEW_ENTRY_ID: ProjectEntryId = ProjectEntryId::MAX;

pub struct ProjectPanel {
    project: Model<Project>,
    fs: Arc<dyn Fs>,
    list: UniformListScrollHandle,
    focus_handle: FocusHandle,
    visible_entries: Vec<(WorktreeId, Vec<Entry>)>,
    last_worktree_root_id: Option<ProjectEntryId>,
    expanded_dir_ids: HashMap<WorktreeId, Vec<ProjectEntryId>>,
    selection: Option<Selection>,
    context_menu: Option<(View<ContextMenu>, Point<Pixels>, Subscription)>,
    edit_state: Option<EditState>,
    filename_editor: View<Editor>,
    clipboard_entry: Option<ClipboardEntry>,
    _dragged_entry_destination: Option<Arc<Path>>,
    _workspace: WeakView<Workspace>,
    width: Option<f32>,
    pending_serialization: Task<Option<()>>,
}

#[derive(Copy, Clone, Debug)]
struct Selection {
    worktree_id: WorktreeId,
    entry_id: ProjectEntryId,
}

#[derive(Clone, Debug)]
struct EditState {
    worktree_id: WorktreeId,
    entry_id: ProjectEntryId,
    is_new_entry: bool,
    is_dir: bool,
    processing_filename: Option<String>,
}

#[derive(Copy, Clone)]
pub enum ClipboardEntry {
    Copied {
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
    },
    Cut {
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct EntryDetails {
    filename: String,
    icon: Option<Arc<str>>,
    path: Arc<Path>,
    depth: usize,
    kind: EntryKind,
    is_ignored: bool,
    is_expanded: bool,
    is_selected: bool,
    is_editing: bool,
    is_processing: bool,
    is_cut: bool,
    git_status: Option<GitFileStatus>,
}

actions!(
    ExpandSelectedEntry,
    CollapseSelectedEntry,
    CollapseAllEntries,
    NewDirectory,
    NewFile,
    Copy,
    CopyPath,
    CopyRelativePath,
    RevealInFinder,
    OpenInTerminal,
    Cut,
    Paste,
    Delete,
    Rename,
    Open,
    ToggleFocus,
    NewSearchInDirectory,
);

pub fn init_settings(cx: &mut AppContext) {
    ProjectPanelSettings::register(cx);
}

pub fn init(assets: impl AssetSource, cx: &mut AppContext) {
    init_settings(cx);
    file_associations::init(assets, cx);

    cx.observe_new_views(|workspace: &mut Workspace, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, cx| {
            workspace.toggle_panel_focus::<ProjectPanel>(cx);
        });
    })
    .detach();
}

#[derive(Debug)]
pub enum Event {
    OpenedEntry {
        entry_id: ProjectEntryId,
        focus_opened_item: bool,
    },
    SplitEntry {
        entry_id: ProjectEntryId,
    },
    Focus,
    NewSearchInDirectory {
        dir_entry: Entry,
    },
    ActivatePanel,
}

#[derive(Serialize, Deserialize)]
struct SerializedProjectPanel {
    width: Option<f32>,
}

impl ProjectPanel {
    fn new(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) -> View<Self> {
        let project = workspace.project().clone();
        let project_panel = cx.build_view(|cx: &mut ViewContext<Self>| {
            cx.observe(&project, |this, _, cx| {
                this.update_visible_entries(None, cx);
                cx.notify();
            })
            .detach();
            let focus_handle = cx.focus_handle();

            cx.on_focus(&focus_handle, Self::focus_in).detach();

            cx.subscribe(&project, |this, project, event, cx| match event {
                project::Event::ActiveEntryChanged(Some(entry_id)) => {
                    if let Some(worktree_id) = project.read(cx).worktree_id_for_entry(*entry_id, cx)
                    {
                        this.expand_entry(worktree_id, *entry_id, cx);
                        this.update_visible_entries(Some((worktree_id, *entry_id)), cx);
                        this.autoscroll(cx);
                        cx.notify();
                    }
                }
                project::Event::ActivateProjectPanel => {
                    cx.emit(Event::ActivatePanel);
                }
                project::Event::WorktreeRemoved(id) => {
                    this.expanded_dir_ids.remove(id);
                    this.update_visible_entries(None, cx);
                    cx.notify();
                }
                _ => {}
            })
            .detach();

            let filename_editor = cx.build_view(|cx| Editor::single_line(cx));

            cx.subscribe(&filename_editor, |this, _, event, cx| match event {
                editor::EditorEvent::BufferEdited
                | editor::EditorEvent::SelectionsChanged { .. } => {
                    this.autoscroll(cx);
                }
                editor::EditorEvent::Blurred => {
                    if this
                        .edit_state
                        .as_ref()
                        .map_or(false, |state| state.processing_filename.is_none())
                    {
                        this.edit_state = None;
                        this.update_visible_entries(None, cx);
                    }
                }
                _ => {}
            })
            .detach();

            // cx.observe_global::<FileAssociations, _>(|_, cx| {
            //     cx.notify();
            // })
            // .detach();

            let mut this = Self {
                project: project.clone(),
                fs: workspace.app_state().fs.clone(),
                list: UniformListScrollHandle::new(),
                focus_handle,
                visible_entries: Default::default(),
                last_worktree_root_id: Default::default(),
                expanded_dir_ids: Default::default(),
                selection: None,
                edit_state: None,
                context_menu: None,
                filename_editor,
                clipboard_entry: None,
                // context_menu: cx.add_view(|cx| ContextMenu::new(view_id, cx)),
                _dragged_entry_destination: None,
                _workspace: workspace.weak_handle(),
                width: None,
                pending_serialization: Task::ready(None),
            };
            this.update_visible_entries(None, cx);

            // Update the dock position when the setting changes.
            let mut old_dock_position = this.position(cx);
            ProjectPanelSettings::register(cx);
            cx.observe_global::<SettingsStore>(move |this, cx| {
                let new_dock_position = this.position(cx);
                if new_dock_position != old_dock_position {
                    old_dock_position = new_dock_position;
                    cx.emit(PanelEvent::ChangePosition);
                }
            })
            .detach();

            this
        });

        cx.subscribe(&project_panel, {
            let project_panel = project_panel.downgrade();
            move |workspace, _, event, cx| match event {
                &Event::OpenedEntry {
                    entry_id,
                    focus_opened_item,
                } => {
                    if let Some(worktree) = project.read(cx).worktree_for_entry(entry_id, cx) {
                        if let Some(entry) = worktree.read(cx).entry_for_id(entry_id) {
                            workspace
                                .open_path(
                                    ProjectPath {
                                        worktree_id: worktree.read(cx).id(),
                                        path: entry.path.clone(),
                                    },
                                    None,
                                    focus_opened_item,
                                    cx,
                                )
                                .detach_and_log_err(cx);
                            if !focus_opened_item {
                                if let Some(project_panel) = project_panel.upgrade() {
                                    let focus_handle = project_panel.read(cx).focus_handle.clone();
                                    cx.focus(&focus_handle);
                                }
                            }
                        }
                    }
                }
                &Event::SplitEntry { entry_id } => {
                    if let Some(worktree) = project.read(cx).worktree_for_entry(entry_id, cx) {
                        if let Some(_entry) = worktree.read(cx).entry_for_id(entry_id) {
                            // workspace
                            //     .split_path(
                            //         ProjectPath {
                            //             worktree_id: worktree.read(cx).id(),
                            //             path: entry.path.clone(),
                            //         },
                            //         cx,
                            //     )
                            //     .detach_and_log_err(cx);
                        }
                    }
                }
                _ => {}
            }
        })
        .detach();

        project_panel
    }

    pub async fn load(
        workspace: WeakView<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<View<Self>> {
        let serialized_panel = cx
            .background_executor()
            .spawn(async move { KEY_VALUE_STORE.read_kvp(PROJECT_PANEL_KEY) })
            .await
            .map_err(|e| anyhow!("Failed to load project panel: {}", e))
            .log_err()
            .flatten()
            .map(|panel| serde_json::from_str::<SerializedProjectPanel>(&panel))
            .transpose()
            .log_err()
            .flatten();

        workspace.update(&mut cx, |workspace, cx| {
            let panel = ProjectPanel::new(workspace, cx);
            if let Some(serialized_panel) = serialized_panel {
                panel.update(cx, |panel, cx| {
                    panel.width = serialized_panel.width;
                    cx.notify();
                });
            }
            panel
        })
    }

    fn serialize(&mut self, cx: &mut ViewContext<Self>) {
        let width = self.width;
        self.pending_serialization = cx.background_executor().spawn(
            async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        PROJECT_PANEL_KEY.into(),
                        serde_json::to_string(&SerializedProjectPanel { width })?,
                    )
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn focus_in(&mut self, cx: &mut ViewContext<Self>) {
        if !self.focus_handle.contains_focused(cx) {
            cx.emit(Event::Focus);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry_id: ProjectEntryId,
        cx: &mut ViewContext<Self>,
    ) {
        let this = cx.view().clone();
        let project = self.project.read(cx);

        let worktree_id = if let Some(id) = project.worktree_id_for_entry(entry_id, cx) {
            id
        } else {
            return;
        };

        self.selection = Some(Selection {
            worktree_id,
            entry_id,
        });

        if let Some((worktree, entry)) = self.selected_entry(cx) {
            let is_root = Some(entry) == worktree.root_entry();
            let is_dir = entry.is_dir();
            let worktree_id = worktree.id();
            let is_local = project.is_local();

            let context_menu = ContextMenu::build(cx, |mut menu, cx| {
                if is_local {
                    menu = menu.action(
                        "Add Folder to Project",
                        Box::new(workspace::AddFolderToProject),
                    );
                    if is_root {
                        menu = menu.entry(
                            "Remove from Project",
                            cx.handler_for(&this, move |this, cx| {
                                this.project.update(cx, |project, cx| {
                                    project.remove_worktree(worktree_id, cx)
                                });
                            }),
                        );
                    }
                }

                menu = menu
                    .action("New File", Box::new(NewFile))
                    .action("New Folder", Box::new(NewDirectory))
                    .separator()
                    .action("Cut", Box::new(Cut))
                    .action("Copy", Box::new(Copy));

                if let Some(clipboard_entry) = self.clipboard_entry {
                    if clipboard_entry.worktree_id() == worktree_id {
                        menu = menu.action("Paste", Box::new(Paste));
                    }
                }

                menu = menu
                    .separator()
                    .action("Copy Path", Box::new(CopyPath))
                    .action("Copy Relative Path", Box::new(CopyRelativePath))
                    .separator()
                    .action("Reveal in Finder", Box::new(RevealInFinder));

                if is_dir {
                    menu = menu
                        .action("Open in Terminal", Box::new(OpenInTerminal))
                        .action("Search Inside", Box::new(NewSearchInDirectory))
                }

                menu = menu.separator().action("Rename", Box::new(Rename));

                if !is_root {
                    menu = menu.action("Delete", Box::new(Delete));
                }

                menu
            });

            cx.focus_view(&context_menu);
            let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
                this.context_menu.take();
                cx.notify();
            });
            self.context_menu = Some((context_menu, position, subscription));
        }

        cx.notify();
    }

    fn expand_selected_entry(&mut self, _: &ExpandSelectedEntry, cx: &mut ViewContext<Self>) {
        if let Some((worktree, entry)) = self.selected_entry(cx) {
            if entry.is_dir() {
                let worktree_id = worktree.id();
                let entry_id = entry.id;
                let expanded_dir_ids =
                    if let Some(expanded_dir_ids) = self.expanded_dir_ids.get_mut(&worktree_id) {
                        expanded_dir_ids
                    } else {
                        return;
                    };

                match expanded_dir_ids.binary_search(&entry_id) {
                    Ok(_) => self.select_next(&SelectNext, cx),
                    Err(ix) => {
                        self.project.update(cx, |project, cx| {
                            project.expand_entry(worktree_id, entry_id, cx);
                        });

                        expanded_dir_ids.insert(ix, entry_id);
                        self.update_visible_entries(None, cx);
                        cx.notify();
                    }
                }
            }
        }
    }

    fn collapse_selected_entry(&mut self, _: &CollapseSelectedEntry, cx: &mut ViewContext<Self>) {
        if let Some((worktree, mut entry)) = self.selected_entry(cx) {
            let worktree_id = worktree.id();
            let expanded_dir_ids =
                if let Some(expanded_dir_ids) = self.expanded_dir_ids.get_mut(&worktree_id) {
                    expanded_dir_ids
                } else {
                    return;
                };

            loop {
                let entry_id = entry.id;
                match expanded_dir_ids.binary_search(&entry_id) {
                    Ok(ix) => {
                        expanded_dir_ids.remove(ix);
                        self.update_visible_entries(Some((worktree_id, entry_id)), cx);
                        cx.notify();
                        break;
                    }
                    Err(_) => {
                        if let Some(parent_entry) =
                            entry.path.parent().and_then(|p| worktree.entry_for_path(p))
                        {
                            entry = parent_entry;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }

    pub fn collapse_all_entries(&mut self, _: &CollapseAllEntries, cx: &mut ViewContext<Self>) {
        self.expanded_dir_ids.clear();
        self.update_visible_entries(None, cx);
        cx.notify();
    }

    fn toggle_expanded(&mut self, entry_id: ProjectEntryId, cx: &mut ViewContext<Self>) {
        if let Some(worktree_id) = self.project.read(cx).worktree_id_for_entry(entry_id, cx) {
            if let Some(expanded_dir_ids) = self.expanded_dir_ids.get_mut(&worktree_id) {
                self.project.update(cx, |project, cx| {
                    match expanded_dir_ids.binary_search(&entry_id) {
                        Ok(ix) => {
                            expanded_dir_ids.remove(ix);
                        }
                        Err(ix) => {
                            project.expand_entry(worktree_id, entry_id, cx);
                            expanded_dir_ids.insert(ix, entry_id);
                        }
                    }
                });
                self.update_visible_entries(Some((worktree_id, entry_id)), cx);
                cx.focus(&self.focus_handle);
                cx.notify();
            }
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, cx: &mut ViewContext<Self>) {
        if let Some(selection) = self.selection {
            let (mut worktree_ix, mut entry_ix, _) =
                self.index_for_selection(selection).unwrap_or_default();
            if entry_ix > 0 {
                entry_ix -= 1;
            } else if worktree_ix > 0 {
                worktree_ix -= 1;
                entry_ix = self.visible_entries[worktree_ix].1.len() - 1;
            } else {
                return;
            }

            let (worktree_id, worktree_entries) = &self.visible_entries[worktree_ix];
            self.selection = Some(Selection {
                worktree_id: *worktree_id,
                entry_id: worktree_entries[entry_ix].id,
            });
            self.autoscroll(cx);
            cx.notify();
        } else {
            self.select_first(cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, cx: &mut ViewContext<Self>) {
        if let Some(task) = self.confirm_edit(cx) {
            task.detach_and_log_err(cx);
        }
    }

    fn open_file(&mut self, _: &Open, cx: &mut ViewContext<Self>) {
        if let Some((_, entry)) = self.selected_entry(cx) {
            if entry.is_file() {
                self.open_entry(entry.id, true, cx);
            }
        }
    }

    fn confirm_edit(&mut self, cx: &mut ViewContext<Self>) -> Option<Task<Result<()>>> {
        let edit_state = self.edit_state.as_mut()?;
        cx.focus(&self.focus_handle);

        let worktree_id = edit_state.worktree_id;
        let is_new_entry = edit_state.is_new_entry;
        let is_dir = edit_state.is_dir;
        let worktree = self.project.read(cx).worktree_for_id(worktree_id, cx)?;
        let entry = worktree.read(cx).entry_for_id(edit_state.entry_id)?.clone();
        let filename = self.filename_editor.read(cx).text(cx);

        let path_already_exists = |path| worktree.read(cx).entry_for_path(path).is_some();
        let edit_task;
        let edited_entry_id;
        if is_new_entry {
            self.selection = Some(Selection {
                worktree_id,
                entry_id: NEW_ENTRY_ID,
            });
            let new_path = entry.path.join(&filename.trim_start_matches("/"));
            if path_already_exists(new_path.as_path()) {
                return None;
            }

            edited_entry_id = NEW_ENTRY_ID;
            edit_task = self.project.update(cx, |project, cx| {
                project.create_entry((worktree_id, &new_path), is_dir, cx)
            });
        } else {
            let new_path = if let Some(parent) = entry.path.clone().parent() {
                parent.join(&filename)
            } else {
                filename.clone().into()
            };
            if path_already_exists(new_path.as_path()) {
                return None;
            }

            edited_entry_id = entry.id;
            edit_task = self.project.update(cx, |project, cx| {
                project.rename_entry(entry.id, new_path.as_path(), cx)
            });
        };

        edit_state.processing_filename = Some(filename);
        cx.notify();

        Some(cx.spawn(|this, mut cx| async move {
            let new_entry = edit_task.await;
            this.update(&mut cx, |this, cx| {
                this.edit_state.take();
                cx.notify();
            })?;

            if let Some(new_entry) = new_entry? {
                this.update(&mut cx, |this, cx| {
                    if let Some(selection) = &mut this.selection {
                        if selection.entry_id == edited_entry_id {
                            selection.worktree_id = worktree_id;
                            selection.entry_id = new_entry.id;
                            this.expand_to_selection(cx);
                        }
                    }
                    this.update_visible_entries(None, cx);
                    if is_new_entry && !is_dir {
                        this.open_entry(new_entry.id, true, cx);
                    }
                    cx.notify();
                })?;
            }
            Ok(())
        }))
    }

    fn cancel(&mut self, _: &Cancel, cx: &mut ViewContext<Self>) {
        self.edit_state = None;
        self.update_visible_entries(None, cx);
        cx.focus(&self.focus_handle);
        cx.notify();
    }

    fn open_entry(
        &mut self,
        entry_id: ProjectEntryId,
        focus_opened_item: bool,
        cx: &mut ViewContext<Self>,
    ) {
        cx.emit(Event::OpenedEntry {
            entry_id,
            focus_opened_item,
        });
    }

    fn split_entry(&mut self, entry_id: ProjectEntryId, cx: &mut ViewContext<Self>) {
        cx.emit(Event::SplitEntry { entry_id });
    }

    fn new_file(&mut self, _: &NewFile, cx: &mut ViewContext<Self>) {
        self.add_entry(false, cx)
    }

    fn new_directory(&mut self, _: &NewDirectory, cx: &mut ViewContext<Self>) {
        self.add_entry(true, cx)
    }

    fn add_entry(&mut self, is_dir: bool, cx: &mut ViewContext<Self>) {
        if let Some(Selection {
            worktree_id,
            entry_id,
        }) = self.selection
        {
            let directory_id;
            if let Some((worktree, expanded_dir_ids)) = self
                .project
                .read(cx)
                .worktree_for_id(worktree_id, cx)
                .zip(self.expanded_dir_ids.get_mut(&worktree_id))
            {
                let worktree = worktree.read(cx);
                if let Some(mut entry) = worktree.entry_for_id(entry_id) {
                    loop {
                        if entry.is_dir() {
                            if let Err(ix) = expanded_dir_ids.binary_search(&entry.id) {
                                expanded_dir_ids.insert(ix, entry.id);
                            }
                            directory_id = entry.id;
                            break;
                        } else {
                            if let Some(parent_path) = entry.path.parent() {
                                if let Some(parent_entry) = worktree.entry_for_path(parent_path) {
                                    entry = parent_entry;
                                    continue;
                                }
                            }
                            return;
                        }
                    }
                } else {
                    return;
                };
            } else {
                return;
            };

            self.edit_state = Some(EditState {
                worktree_id,
                entry_id: directory_id,
                is_new_entry: true,
                is_dir,
                processing_filename: None,
            });
            self.filename_editor.update(cx, |editor, cx| {
                editor.clear(cx);
                editor.focus(cx);
            });
            self.update_visible_entries(Some((worktree_id, NEW_ENTRY_ID)), cx);
            self.autoscroll(cx);
            cx.notify();
        }
    }

    fn rename(&mut self, _: &Rename, cx: &mut ViewContext<Self>) {
        if let Some(Selection {
            worktree_id,
            entry_id,
        }) = self.selection
        {
            if let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) {
                if let Some(entry) = worktree.read(cx).entry_for_id(entry_id) {
                    self.edit_state = Some(EditState {
                        worktree_id,
                        entry_id,
                        is_new_entry: false,
                        is_dir: entry.is_dir(),
                        processing_filename: None,
                    });
                    let file_name = entry
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default()
                        .to_string();
                    let file_stem = entry.path.file_stem().map(|s| s.to_string_lossy());
                    let selection_end =
                        file_stem.map_or(file_name.len(), |file_stem| file_stem.len());
                    self.filename_editor.update(cx, |editor, cx| {
                        editor.set_text(file_name, cx);
                        editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                            s.select_ranges([0..selection_end])
                        });
                        editor.focus(cx);
                    });
                    self.update_visible_entries(None, cx);
                    self.autoscroll(cx);
                    cx.notify();
                }
            }

            // cx.update_global(|drag_and_drop: &mut DragAndDrop<Workspace>, cx| {
            //     drag_and_drop.cancel_dragging::<ProjectEntryId>(cx);
            // })
        }
    }

    fn delete(&mut self, _: &Delete, cx: &mut ViewContext<Self>) {
        maybe!({
            let Selection { entry_id, .. } = self.selection?;
            let path = self.project.read(cx).path_for_entry(entry_id, cx)?.path;
            let file_name = path.file_name()?;

            let answer = cx.prompt(
                PromptLevel::Info,
                &format!("Delete {file_name:?}?"),
                &["Delete", "Cancel"],
            );

            cx.spawn(|this, mut cx| async move {
                if answer.await != Ok(0) {
                    return Ok(());
                }
                this.update(&mut cx, |this, cx| {
                    this.project
                        .update(cx, |project, cx| project.delete_entry(entry_id, cx))
                        .ok_or_else(|| anyhow!("no such entry"))
                })??
                .await
            })
            .detach_and_log_err(cx);
            Some(())
        });
    }

    fn select_next(&mut self, _: &SelectNext, cx: &mut ViewContext<Self>) {
        if let Some(selection) = self.selection {
            let (mut worktree_ix, mut entry_ix, _) =
                self.index_for_selection(selection).unwrap_or_default();
            if let Some((_, worktree_entries)) = self.visible_entries.get(worktree_ix) {
                if entry_ix + 1 < worktree_entries.len() {
                    entry_ix += 1;
                } else {
                    worktree_ix += 1;
                    entry_ix = 0;
                }
            }

            if let Some((worktree_id, worktree_entries)) = self.visible_entries.get(worktree_ix) {
                if let Some(entry) = worktree_entries.get(entry_ix) {
                    self.selection = Some(Selection {
                        worktree_id: *worktree_id,
                        entry_id: entry.id,
                    });
                    self.autoscroll(cx);
                    cx.notify();
                }
            }
        } else {
            self.select_first(cx);
        }
    }

    fn select_first(&mut self, cx: &mut ViewContext<Self>) {
        let worktree = self
            .visible_entries
            .first()
            .and_then(|(worktree_id, _)| self.project.read(cx).worktree_for_id(*worktree_id, cx));
        if let Some(worktree) = worktree {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            if let Some(root_entry) = worktree.root_entry() {
                self.selection = Some(Selection {
                    worktree_id,
                    entry_id: root_entry.id,
                });
                self.autoscroll(cx);
                cx.notify();
            }
        }
    }

    fn autoscroll(&mut self, cx: &mut ViewContext<Self>) {
        if let Some((_, _, index)) = self.selection.and_then(|s| self.index_for_selection(s)) {
            self.list.scroll_to_item(index);
            cx.notify();
        }
    }

    fn cut(&mut self, _: &Cut, cx: &mut ViewContext<Self>) {
        if let Some((worktree, entry)) = self.selected_entry(cx) {
            self.clipboard_entry = Some(ClipboardEntry::Cut {
                worktree_id: worktree.id(),
                entry_id: entry.id,
            });
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, cx: &mut ViewContext<Self>) {
        if let Some((worktree, entry)) = self.selected_entry(cx) {
            self.clipboard_entry = Some(ClipboardEntry::Copied {
                worktree_id: worktree.id(),
                entry_id: entry.id,
            });
            cx.notify();
        }
    }

    fn paste(&mut self, _: &Paste, cx: &mut ViewContext<Self>) {
        maybe!({
            let (worktree, entry) = self.selected_entry(cx)?;
            let clipboard_entry = self.clipboard_entry?;
            if clipboard_entry.worktree_id() != worktree.id() {
                return None;
            }

            let clipboard_entry_file_name = self
                .project
                .read(cx)
                .path_for_entry(clipboard_entry.entry_id(), cx)?
                .path
                .file_name()?
                .to_os_string();

            let mut new_path = entry.path.to_path_buf();
            if entry.is_file() {
                new_path.pop();
            }

            new_path.push(&clipboard_entry_file_name);
            let extension = new_path.extension().map(|e| e.to_os_string());
            let file_name_without_extension = Path::new(&clipboard_entry_file_name).file_stem()?;
            let mut ix = 0;
            while worktree.entry_for_path(&new_path).is_some() {
                new_path.pop();

                let mut new_file_name = file_name_without_extension.to_os_string();
                new_file_name.push(" copy");
                if ix > 0 {
                    new_file_name.push(format!(" {}", ix));
                }
                if let Some(extension) = extension.as_ref() {
                    new_file_name.push(".");
                    new_file_name.push(extension);
                }

                new_path.push(new_file_name);
                ix += 1;
            }

            if clipboard_entry.is_cut() {
                self.project
                    .update(cx, |project, cx| {
                        project.rename_entry(clipboard_entry.entry_id(), new_path, cx)
                    })
                    .detach_and_log_err(cx)
            } else {
                self.project
                    .update(cx, |project, cx| {
                        project.copy_entry(clipboard_entry.entry_id(), new_path, cx)
                    })
                    .detach_and_log_err(cx)
            }

            Some(())
        });
    }

    fn copy_path(&mut self, _: &CopyPath, cx: &mut ViewContext<Self>) {
        if let Some((worktree, entry)) = self.selected_entry(cx) {
            cx.write_to_clipboard(ClipboardItem::new(
                worktree
                    .abs_path()
                    .join(&entry.path)
                    .to_string_lossy()
                    .to_string(),
            ));
        }
    }

    fn copy_relative_path(&mut self, _: &CopyRelativePath, cx: &mut ViewContext<Self>) {
        if let Some((_, entry)) = self.selected_entry(cx) {
            cx.write_to_clipboard(ClipboardItem::new(entry.path.to_string_lossy().to_string()));
        }
    }

    fn reveal_in_finder(&mut self, _: &RevealInFinder, cx: &mut ViewContext<Self>) {
        if let Some((worktree, entry)) = self.selected_entry(cx) {
            cx.reveal_path(&worktree.abs_path().join(&entry.path));
        }
    }

    fn open_in_terminal(&mut self, _: &OpenInTerminal, _cx: &mut ViewContext<Self>) {
        todo!()
        // if let Some((worktree, entry)) = self.selected_entry(cx) {
        //     let window = cx.window();
        //     let view_id = cx.view_id();
        //     let path = worktree.abs_path().join(&entry.path);

        //     cx.app_context()
        //         .spawn(|mut cx| async move {
        //             window.dispatch_action(
        //                 view_id,
        //                 &workspace::OpenTerminal {
        //                     working_directory: path,
        //                 },
        //                 &mut cx,
        //             );
        //         })
        //         .detach();
        // }
    }

    pub fn new_search_in_directory(
        &mut self,
        _: &NewSearchInDirectory,
        cx: &mut ViewContext<Self>,
    ) {
        if let Some((_, entry)) = self.selected_entry(cx) {
            if entry.is_dir() {
                cx.emit(Event::NewSearchInDirectory {
                    dir_entry: entry.clone(),
                });
            }
        }
    }

    // todo!()
    // fn move_entry(
    //     &mut self,
    //     entry_to_move: ProjectEntryId,
    //     destination: ProjectEntryId,
    //     destination_is_file: bool,
    //     cx: &mut ViewContext<Self>,
    // ) {
    //     let destination_worktree = self.project.update(cx, |project, cx| {
    //         let entry_path = project.path_for_entry(entry_to_move, cx)?;
    //         let destination_entry_path = project.path_for_entry(destination, cx)?.path.clone();

    //         let mut destination_path = destination_entry_path.as_ref();
    //         if destination_is_file {
    //             destination_path = destination_path.parent()?;
    //         }

    //         let mut new_path = destination_path.to_path_buf();
    //         new_path.push(entry_path.path.file_name()?);
    //         if new_path != entry_path.path.as_ref() {
    //             let task = project.rename_entry(entry_to_move, new_path, cx);
    //             cx.foreground_executor().spawn(task).detach_and_log_err(cx);
    //         }

    //         Some(project.worktree_id_for_entry(destination, cx)?)
    //     });

    //     if let Some(destination_worktree) = destination_worktree {
    //         self.expand_entry(destination_worktree, destination, cx);
    //     }
    // }

    fn index_for_selection(&self, selection: Selection) -> Option<(usize, usize, usize)> {
        let mut entry_index = 0;
        let mut visible_entries_index = 0;
        for (worktree_index, (worktree_id, worktree_entries)) in
            self.visible_entries.iter().enumerate()
        {
            if *worktree_id == selection.worktree_id {
                for entry in worktree_entries {
                    if entry.id == selection.entry_id {
                        return Some((worktree_index, entry_index, visible_entries_index));
                    } else {
                        visible_entries_index += 1;
                        entry_index += 1;
                    }
                }
                break;
            } else {
                visible_entries_index += worktree_entries.len();
            }
        }
        None
    }

    pub fn selected_entry<'a>(
        &self,
        cx: &'a AppContext,
    ) -> Option<(&'a Worktree, &'a project::Entry)> {
        let (worktree, entry) = self.selected_entry_handle(cx)?;
        Some((worktree.read(cx), entry))
    }

    fn selected_entry_handle<'a>(
        &self,
        cx: &'a AppContext,
    ) -> Option<(Model<Worktree>, &'a project::Entry)> {
        let selection = self.selection?;
        let project = self.project.read(cx);
        let worktree = project.worktree_for_id(selection.worktree_id, cx)?;
        let entry = worktree.read(cx).entry_for_id(selection.entry_id)?;
        Some((worktree, entry))
    }

    fn expand_to_selection(&mut self, cx: &mut ViewContext<Self>) -> Option<()> {
        let (worktree, entry) = self.selected_entry(cx)?;
        let expanded_dir_ids = self.expanded_dir_ids.entry(worktree.id()).or_default();

        for path in entry.path.ancestors() {
            let Some(entry) = worktree.entry_for_path(path) else {
                continue;
            };
            if entry.is_dir() {
                if let Err(idx) = expanded_dir_ids.binary_search(&entry.id) {
                    expanded_dir_ids.insert(idx, entry.id);
                }
            }
        }

        Some(())
    }

    fn update_visible_entries(
        &mut self,
        new_selected_entry: Option<(WorktreeId, ProjectEntryId)>,
        cx: &mut ViewContext<Self>,
    ) {
        let project = self.project.read(cx);
        self.last_worktree_root_id = project
            .visible_worktrees(cx)
            .rev()
            .next()
            .and_then(|worktree| worktree.read(cx).root_entry())
            .map(|entry| entry.id);

        self.visible_entries.clear();
        for worktree in project.visible_worktrees(cx) {
            let snapshot = worktree.read(cx).snapshot();
            let worktree_id = snapshot.id();

            let expanded_dir_ids = match self.expanded_dir_ids.entry(worktree_id) {
                hash_map::Entry::Occupied(e) => e.into_mut(),
                hash_map::Entry::Vacant(e) => {
                    // The first time a worktree's root entry becomes available,
                    // mark that root entry as expanded.
                    if let Some(entry) = snapshot.root_entry() {
                        e.insert(vec![entry.id]).as_slice()
                    } else {
                        &[]
                    }
                }
            };

            let mut new_entry_parent_id = None;
            let mut new_entry_kind = EntryKind::Dir;
            if let Some(edit_state) = &self.edit_state {
                if edit_state.worktree_id == worktree_id && edit_state.is_new_entry {
                    new_entry_parent_id = Some(edit_state.entry_id);
                    new_entry_kind = if edit_state.is_dir {
                        EntryKind::Dir
                    } else {
                        EntryKind::File(Default::default())
                    };
                }
            }

            let mut visible_worktree_entries = Vec::new();
            let mut entry_iter = snapshot.entries(true);

            while let Some(entry) = entry_iter.entry() {
                visible_worktree_entries.push(entry.clone());
                if Some(entry.id) == new_entry_parent_id {
                    visible_worktree_entries.push(Entry {
                        id: NEW_ENTRY_ID,
                        kind: new_entry_kind,
                        path: entry.path.join("\0").into(),
                        inode: 0,
                        mtime: entry.mtime,
                        is_symlink: false,
                        is_ignored: false,
                        is_external: false,
                        git_status: entry.git_status,
                    });
                }
                if expanded_dir_ids.binary_search(&entry.id).is_err()
                    && entry_iter.advance_to_sibling()
                {
                    continue;
                }
                entry_iter.advance();
            }

            snapshot.propagate_git_statuses(&mut visible_worktree_entries);

            visible_worktree_entries.sort_by(|entry_a, entry_b| {
                let mut components_a = entry_a.path.components().peekable();
                let mut components_b = entry_b.path.components().peekable();
                loop {
                    match (components_a.next(), components_b.next()) {
                        (Some(component_a), Some(component_b)) => {
                            let a_is_file = components_a.peek().is_none() && entry_a.is_file();
                            let b_is_file = components_b.peek().is_none() && entry_b.is_file();
                            let ordering = a_is_file.cmp(&b_is_file).then_with(|| {
                                let name_a =
                                    UniCase::new(component_a.as_os_str().to_string_lossy());
                                let name_b =
                                    UniCase::new(component_b.as_os_str().to_string_lossy());
                                name_a.cmp(&name_b)
                            });
                            if !ordering.is_eq() {
                                return ordering;
                            }
                        }
                        (Some(_), None) => break Ordering::Greater,
                        (None, Some(_)) => break Ordering::Less,
                        (None, None) => break Ordering::Equal,
                    }
                }
            });
            self.visible_entries
                .push((worktree_id, visible_worktree_entries));
        }

        if let Some((worktree_id, entry_id)) = new_selected_entry {
            self.selection = Some(Selection {
                worktree_id,
                entry_id,
            });
        }
    }

    fn expand_entry(
        &mut self,
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
        cx: &mut ViewContext<Self>,
    ) {
        self.project.update(cx, |project, cx| {
            if let Some((worktree, expanded_dir_ids)) = project
                .worktree_for_id(worktree_id, cx)
                .zip(self.expanded_dir_ids.get_mut(&worktree_id))
            {
                project.expand_entry(worktree_id, entry_id, cx);
                let worktree = worktree.read(cx);

                if let Some(mut entry) = worktree.entry_for_id(entry_id) {
                    loop {
                        if let Err(ix) = expanded_dir_ids.binary_search(&entry.id) {
                            expanded_dir_ids.insert(ix, entry.id);
                        }

                        if let Some(parent_entry) =
                            entry.path.parent().and_then(|p| worktree.entry_for_path(p))
                        {
                            entry = parent_entry;
                        } else {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn for_each_visible_entry(
        &self,
        range: Range<usize>,
        cx: &mut ViewContext<ProjectPanel>,
        mut callback: impl FnMut(ProjectEntryId, EntryDetails, &mut ViewContext<ProjectPanel>),
    ) {
        let mut ix = 0;
        for (worktree_id, visible_worktree_entries) in &self.visible_entries {
            if ix >= range.end {
                return;
            }

            if ix + visible_worktree_entries.len() <= range.start {
                ix += visible_worktree_entries.len();
                continue;
            }

            let end_ix = range.end.min(ix + visible_worktree_entries.len());
            let (git_status_setting, show_file_icons, show_folder_icons) = {
                let settings = ProjectPanelSettings::get_global(cx);
                (
                    settings.git_status,
                    settings.file_icons,
                    settings.folder_icons,
                )
            };
            if let Some(worktree) = self.project.read(cx).worktree_for_id(*worktree_id, cx) {
                let snapshot = worktree.read(cx).snapshot();
                let root_name = OsStr::new(snapshot.root_name());
                let expanded_entry_ids = self
                    .expanded_dir_ids
                    .get(&snapshot.id())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                let entry_range = range.start.saturating_sub(ix)..end_ix - ix;
                for entry in visible_worktree_entries[entry_range].iter() {
                    let status = git_status_setting.then(|| entry.git_status).flatten();
                    let is_expanded = expanded_entry_ids.binary_search(&entry.id).is_ok();
                    let icon = match entry.kind {
                        EntryKind::File(_) => {
                            if show_file_icons {
                                FileAssociations::get_icon(&entry.path, cx)
                            } else {
                                None
                            }
                        }
                        _ => {
                            if show_folder_icons {
                                FileAssociations::get_folder_icon(is_expanded, cx)
                            } else {
                                FileAssociations::get_chevron_icon(is_expanded, cx)
                            }
                        }
                    };

                    let mut details = EntryDetails {
                        filename: entry
                            .path
                            .file_name()
                            .unwrap_or(root_name)
                            .to_string_lossy()
                            .to_string(),
                        icon,
                        path: entry.path.clone(),
                        depth: entry.path.components().count(),
                        kind: entry.kind,
                        is_ignored: entry.is_ignored,
                        is_expanded,
                        is_selected: self.selection.map_or(false, |e| {
                            e.worktree_id == snapshot.id() && e.entry_id == entry.id
                        }),
                        is_editing: false,
                        is_processing: false,
                        is_cut: self
                            .clipboard_entry
                            .map_or(false, |e| e.is_cut() && e.entry_id() == entry.id),
                        git_status: status,
                    };

                    if let Some(edit_state) = &self.edit_state {
                        let is_edited_entry = if edit_state.is_new_entry {
                            entry.id == NEW_ENTRY_ID
                        } else {
                            entry.id == edit_state.entry_id
                        };

                        if is_edited_entry {
                            if let Some(processing_filename) = &edit_state.processing_filename {
                                details.is_processing = true;
                                details.filename.clear();
                                details.filename.push_str(processing_filename);
                            } else {
                                if edit_state.is_new_entry {
                                    details.filename.clear();
                                }
                                details.is_editing = true;
                            }
                        }
                    }

                    callback(entry.id, details, cx);
                }
            }
            ix = end_ix;
        }
    }

    fn render_entry(
        &self,
        entry_id: ProjectEntryId,
        details: EntryDetails,
        // dragged_entry_destination: &mut Option<Arc<Path>>,
        cx: &mut ViewContext<Self>,
    ) -> ListItem {
        let kind = details.kind;
        let settings = ProjectPanelSettings::get_global(cx);
        let show_editor = details.is_editing && !details.is_processing;
        let is_selected = self
            .selection
            .map_or(false, |selection| selection.entry_id == entry_id);

        let theme = cx.theme();
        let filename_text_color = details
            .git_status
            .as_ref()
            .map(|status| match status {
                GitFileStatus::Added => theme.status().created,
                GitFileStatus::Modified => theme.status().modified,
                GitFileStatus::Conflict => theme.status().conflict,
            })
            .unwrap_or(theme.status().info);

        ListItem::new(entry_id.to_proto() as usize)
            .indent_level(details.depth)
            .indent_step_size(px(settings.indent_size))
            .selected(is_selected)
            .child(if let Some(icon) = &details.icon {
                div().child(IconElement::from_path(icon.to_string()))
            } else {
                div()
            })
            .child(
                if let (Some(editor), true) = (Some(&self.filename_editor), show_editor) {
                    div().h_full().w_full().child(editor.clone())
                } else {
                    div()
                        .text_color(filename_text_color)
                        .child(Label::new(details.filename.clone()))
                }
                .ml_1(),
            )
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, cx| {
                if event.down.button == MouseButton::Right {
                    return;
                }
                if !show_editor {
                    if kind.is_dir() {
                        this.toggle_expanded(entry_id, cx);
                    } else {
                        if event.down.modifiers.command {
                            this.split_entry(entry_id, cx);
                        } else {
                            this.open_entry(entry_id, event.up.click_count > 1, cx);
                        }
                    }
                }
            }))
            .on_secondary_mouse_down(cx.listener(move |this, event: &MouseDownEvent, cx| {
                this.deploy_context_menu(event.position, entry_id, cx);
            }))
        // .on_drop::<ProjectEntryId>(|this, event, cx| {
        //     this.move_entry(
        //         *dragged_entry,
        //         entry_id,
        //         matches!(details.kind, EntryKind::File(_)),
        //         cx,
        //     );
        // })
    }

    fn dispatch_context(&self, cx: &ViewContext<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::default();
        dispatch_context.add("ProjectPanel");
        dispatch_context.add("menu");

        let identifier = if self.filename_editor.focus_handle(cx).is_focused(cx) {
            "editing"
        } else {
            "not_editing"
        };

        dispatch_context.add(identifier);

        dispatch_context
    }
}

impl Render for ProjectPanel {
    type Element = Focusable<Stateful<Div>>;

    fn render(&mut self, cx: &mut gpui::ViewContext<Self>) -> Self::Element {
        let has_worktree = self.visible_entries.len() != 0;

        if has_worktree {
            div()
                .id("project-panel")
                .size_full()
                .relative()
                .key_context(self.dispatch_context(cx))
                .on_action(cx.listener(Self::select_next))
                .on_action(cx.listener(Self::select_prev))
                .on_action(cx.listener(Self::expand_selected_entry))
                .on_action(cx.listener(Self::collapse_selected_entry))
                .on_action(cx.listener(Self::collapse_all_entries))
                .on_action(cx.listener(Self::new_file))
                .on_action(cx.listener(Self::new_directory))
                .on_action(cx.listener(Self::rename))
                .on_action(cx.listener(Self::delete))
                .on_action(cx.listener(Self::confirm))
                .on_action(cx.listener(Self::open_file))
                .on_action(cx.listener(Self::cancel))
                .on_action(cx.listener(Self::cut))
                .on_action(cx.listener(Self::copy))
                .on_action(cx.listener(Self::copy_path))
                .on_action(cx.listener(Self::copy_relative_path))
                .on_action(cx.listener(Self::paste))
                .on_action(cx.listener(Self::reveal_in_finder))
                .on_action(cx.listener(Self::open_in_terminal))
                .on_action(cx.listener(Self::new_search_in_directory))
                .track_focus(&self.focus_handle)
                .child(
                    uniform_list(
                        cx.view().clone(),
                        "entries",
                        self.visible_entries
                            .iter()
                            .map(|(_, worktree_entries)| worktree_entries.len())
                            .sum(),
                        {
                            |this, range, cx| {
                                let mut items = Vec::new();
                                this.for_each_visible_entry(range, cx, |id, details, cx| {
                                    items.push(this.render_entry(id, details, cx));
                                });
                                items
                            }
                        },
                    )
                    .size_full()
                    .track_scroll(self.list.clone()),
                )
                .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                    overlay()
                        .position(*position)
                        .anchor(gpui::AnchorCorner::TopLeft)
                        .child(menu.clone())
                }))
        } else {
            v_stack()
                .id("empty-project_panel")
                .track_focus(&self.focus_handle)
        }
    }
}

impl EventEmitter<Event> for ProjectPanel {}

impl EventEmitter<PanelEvent> for ProjectPanel {}

impl Panel for ProjectPanel {
    fn position(&self, cx: &WindowContext) -> DockPosition {
        match ProjectPanelSettings::get_global(cx).dock {
            ProjectPanelDockPosition::Left => DockPosition::Left,
            ProjectPanelDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, cx: &mut ViewContext<Self>) {
        settings::update_settings_file::<ProjectPanelSettings>(
            self.fs.clone(),
            cx,
            move |settings| {
                let dock = match position {
                    DockPosition::Left | DockPosition::Bottom => ProjectPanelDockPosition::Left,
                    DockPosition::Right => ProjectPanelDockPosition::Right,
                };
                settings.dock = Some(dock);
            },
        );
    }

    fn size(&self, cx: &WindowContext) -> f32 {
        self.width
            .unwrap_or_else(|| ProjectPanelSettings::get_global(cx).default_width)
    }

    fn set_size(&mut self, size: Option<f32>, cx: &mut ViewContext<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, _: &WindowContext) -> Option<ui::Icon> {
        Some(ui::Icon::FileTree)
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn persistent_name() -> &'static str {
        "Project Panel"
    }
}

impl FocusableView for ProjectPanel {
    fn focus_handle(&self, _cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ClipboardEntry {
    fn is_cut(&self) -> bool {
        matches!(self, Self::Cut { .. })
    }

    fn entry_id(&self) -> ProjectEntryId {
        match self {
            ClipboardEntry::Copied { entry_id, .. } | ClipboardEntry::Cut { entry_id, .. } => {
                *entry_id
            }
        }
    }

    fn worktree_id(&self) -> WorktreeId {
        match self {
            ClipboardEntry::Copied { worktree_id, .. }
            | ClipboardEntry::Cut { worktree_id, .. } => *worktree_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, View, VisualTestContext, WindowHandle};
    use pretty_assertions::assert_eq;
    use project::{project_settings::ProjectSettings, FakeFs};
    use serde_json::json;
    use settings::SettingsStore;
    use std::{
        collections::HashSet,
        path::{Path, PathBuf},
        sync::atomic::{self, AtomicUsize},
    };
    use workspace::AppState;

    #[gpui::test]
    async fn test_visible_list(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/root1",
            json!({
                ".dockerignore": "",
                ".git": {
                    "HEAD": "",
                },
                "a": {
                    "0": { "q": "", "r": "", "s": "" },
                    "1": { "t": "", "u": "" },
                    "2": { "v": "", "w": "", "x": "", "y": "" },
                },
                "b": {
                    "3": { "Q": "" },
                    "4": { "R": "", "S": "", "T": "", "U": "" },
                },
                "C": {
                    "5": {},
                    "6": { "V": "", "W": "" },
                    "7": { "X": "" },
                    "8": { "Y": {}, "Z": "" }
                }
            }),
        )
        .await;
        fs.insert_tree(
            "/root2",
            json!({
                "d": {
                    "9": ""
                },
                "e": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root1".as_ref(), "/root2".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        toggle_expand_dir(&panel, "root1/b", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b  <== selected",
                "        > 3",
                "        > 4",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        assert_eq!(
            visible_entries_as_strings(&panel, 6..9, cx),
            &[
                //
                "    > C",
                "      .dockerignore",
                "v root2",
            ]
        );
    }

    #[gpui::test]
    async fn test_exclusions_in_visible_list(cx: &mut gpui::TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings::<ProjectSettings>(cx, |project_settings| {
                    project_settings.file_scan_exclusions =
                        Some(vec!["**/.git".to_string(), "**/4/**".to_string()]);
                });
            });
        });

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/root1",
            json!({
                ".dockerignore": "",
                ".git": {
                    "HEAD": "",
                },
                "a": {
                    "0": { "q": "", "r": "", "s": "" },
                    "1": { "t": "", "u": "" },
                    "2": { "v": "", "w": "", "x": "", "y": "" },
                },
                "b": {
                    "3": { "Q": "" },
                    "4": { "R": "", "S": "", "T": "", "U": "" },
                },
                "C": {
                    "5": {},
                    "6": { "V": "", "W": "" },
                    "7": { "X": "" },
                    "8": { "Y": {}, "Z": "" }
                }
            }),
        )
        .await;
        fs.insert_tree(
            "/root2",
            json!({
                "d": {
                    "4": ""
                },
                "e": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root1".as_ref(), "/root2".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > a",
                "    > b",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        toggle_expand_dir(&panel, "root1/b", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > a",
                "    v b  <== selected",
                "        > 3",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        toggle_expand_dir(&panel, "root2/d", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > a",
                "    v b",
                "        > 3",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    v d  <== selected",
                "    > e",
            ]
        );

        toggle_expand_dir(&panel, "root2/e", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                "v root1",
                "    > a",
                "    v b",
                "        > 3",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    v d",
                "    v e  <== selected",
            ]
        );
    }

    #[gpui::test(iterations = 30)]
    async fn test_editing_files(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/root1",
            json!({
                ".dockerignore": "",
                ".git": {
                    "HEAD": "",
                },
                "a": {
                    "0": { "q": "", "r": "", "s": "" },
                    "1": { "t": "", "u": "" },
                    "2": { "v": "", "w": "", "x": "", "y": "" },
                },
                "b": {
                    "3": { "Q": "" },
                    "4": { "R": "", "S": "", "T": "", "U": "" },
                },
                "C": {
                    "5": {},
                    "6": { "V": "", "W": "" },
                    "7": { "X": "" },
                    "8": { "Y": {}, "Z": "" }
                }
            }),
        )
        .await;
        fs.insert_tree(
            "/root2",
            json!({
                "d": {
                    "9": ""
                },
                "e": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root1".as_ref(), "/root2".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        select_path(&panel, "root1", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1  <== selected",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        // Add a file with the root folder selected. The filename editor is placed
        // before the first file in the root folder.
        panel.update(cx, |panel, cx| panel.new_file(&NewFile, cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.filename_editor.read(cx).is_focused(cx));
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      [EDITOR: '']  <== selected",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        let confirm = panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("the-new-filename", cx));
            panel.confirm_edit(cx).unwrap()
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      [PROCESSING: 'the-new-filename']  <== selected",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        confirm.await.unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      .dockerignore",
                "      the-new-filename  <== selected",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        select_path(&panel, "root1/b", cx);
        panel.update(cx, |panel, cx| panel.new_file(&NewFile, cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          [EDITOR: '']  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        panel
            .update(cx, |panel, cx| {
                panel
                    .filename_editor
                    .update(cx, |editor, cx| editor.set_text("another-filename.txt", cx));
                panel.confirm_edit(cx).unwrap()
            })
            .await
            .unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          another-filename.txt  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        select_path(&panel, "root1/b/another-filename.txt", cx);
        panel.update(cx, |panel, cx| panel.rename(&Rename, cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          [EDITOR: 'another-filename.txt']  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        let confirm = panel.update(cx, |panel, cx| {
            panel.filename_editor.update(cx, |editor, cx| {
                let file_name_selections = editor.selections.all::<usize>(cx);
                assert_eq!(file_name_selections.len(), 1, "File editing should have a single selection, but got: {file_name_selections:?}");
                let file_name_selection = &file_name_selections[0];
                assert_eq!(file_name_selection.start, 0, "Should select the file name from the start");
                assert_eq!(file_name_selection.end, "another-filename".len(), "Should not select file extension");

                editor.set_text("a-different-filename.tar.gz", cx)
            });
            panel.confirm_edit(cx).unwrap()
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          [PROCESSING: 'a-different-filename.tar.gz']  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        confirm.await.unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          a-different-filename.tar.gz  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        panel.update(cx, |panel, cx| panel.rename(&Rename, cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3",
                "        > 4",
                "          [EDITOR: 'a-different-filename.tar.gz']  <== selected",
                "    > C",
                "      .dockerignore",
                "      the-new-filename",
            ]
        );

        panel.update(cx, |panel, cx| {
            panel.filename_editor.update(cx, |editor, cx| {
                let file_name_selections = editor.selections.all::<usize>(cx);
                assert_eq!(file_name_selections.len(), 1, "File editing should have a single selection, but got: {file_name_selections:?}");
                let file_name_selection = &file_name_selections[0];
                assert_eq!(file_name_selection.start, 0, "Should select the file name from the start");
                assert_eq!(file_name_selection.end, "a-different-filename.tar".len(), "Should not select file extension, but still may select anything up to the last dot..");

            });
            panel.cancel(&Cancel, cx)
        });

        panel.update(cx, |panel, cx| panel.new_directory(&NewDirectory, cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > [EDITOR: '']  <== selected",
                "        > 3",
                "        > 4",
                "          a-different-filename.tar.gz",
                "    > C",
                "      .dockerignore",
            ]
        );

        let confirm = panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("new-dir", cx));
            panel.confirm_edit(cx).unwrap()
        });
        panel.update(cx, |panel, cx| panel.select_next(&Default::default(), cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > [PROCESSING: 'new-dir']",
                "        > 3  <== selected",
                "        > 4",
                "          a-different-filename.tar.gz",
                "    > C",
                "      .dockerignore",
            ]
        );

        confirm.await.unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3  <== selected",
                "        > 4",
                "        > new-dir",
                "          a-different-filename.tar.gz",
                "    > C",
                "      .dockerignore",
            ]
        );

        panel.update(cx, |panel, cx| panel.rename(&Default::default(), cx));
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > [EDITOR: '3']  <== selected",
                "        > 4",
                "        > new-dir",
                "          a-different-filename.tar.gz",
                "    > C",
                "      .dockerignore",
            ]
        );

        // Dismiss the rename editor when it loses focus.
        workspace.update(cx, |_, cx| cx.blur()).unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    v b",
                "        > 3  <== selected",
                "        > 4",
                "        > new-dir",
                "          a-different-filename.tar.gz",
                "    > C",
                "      .dockerignore",
            ]
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_adding_directories_via_file(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/root1",
            json!({
                ".dockerignore": "",
                ".git": {
                    "HEAD": "",
                },
                "a": {
                    "0": { "q": "", "r": "", "s": "" },
                    "1": { "t": "", "u": "" },
                    "2": { "v": "", "w": "", "x": "", "y": "" },
                },
                "b": {
                    "3": { "Q": "" },
                    "4": { "R": "", "S": "", "T": "", "U": "" },
                },
                "C": {
                    "5": {},
                    "6": { "V": "", "W": "" },
                    "7": { "X": "" },
                    "8": { "Y": {}, "Z": "" }
                }
            }),
        )
        .await;
        fs.insert_tree(
            "/root2",
            json!({
                "d": {
                    "9": ""
                },
                "e": {}
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root1".as_ref(), "/root2".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        select_path(&panel, "root1", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1  <== selected",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        // Add a file with the root folder selected. The filename editor is placed
        // before the first file in the root folder.
        panel.update(cx, |panel, cx| panel.new_file(&NewFile, cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.filename_editor.read(cx).is_focused(cx));
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      [EDITOR: '']  <== selected",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        let confirm = panel.update(cx, |panel, cx| {
            panel.filename_editor.update(cx, |editor, cx| {
                editor.set_text("/bdir1/dir2/the-new-filename", cx)
            });
            panel.confirm_edit(cx).unwrap()
        });

        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    > C",
                "      [PROCESSING: '/bdir1/dir2/the-new-filename']  <== selected",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );

        confirm.await.unwrap();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..13, cx),
            &[
                "v root1",
                "    > .git",
                "    > a",
                "    > b",
                "    v bdir1",
                "        v dir2",
                "              the-new-filename  <== selected",
                "    > C",
                "      .dockerignore",
                "v root2",
                "    > d",
                "    > e",
            ]
        );
    }

    #[gpui::test]
    async fn test_copy_paste(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/root1",
            json!({
                "one.two.txt": "",
                "one.txt": ""
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/root1".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        panel.update(cx, |panel, cx| {
            panel.select_next(&Default::default(), cx);
            panel.select_next(&Default::default(), cx);
        });

        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                //
                "v root1",
                "      one.two.txt  <== selected",
                "      one.txt",
            ]
        );

        // Regression test - file name is created correctly when
        // the copied file's name contains multiple dots.
        panel.update(cx, |panel, cx| {
            panel.copy(&Default::default(), cx);
            panel.paste(&Default::default(), cx);
        });
        cx.executor().run_until_parked();

        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                //
                "v root1",
                "      one.two copy.txt",
                "      one.two.txt  <== selected",
                "      one.txt",
            ]
        );

        panel.update(cx, |panel, cx| {
            panel.paste(&Default::default(), cx);
        });
        cx.executor().run_until_parked();

        assert_eq!(
            visible_entries_as_strings(&panel, 0..50, cx),
            &[
                //
                "v root1",
                "      one.two copy 1.txt",
                "      one.two copy.txt",
                "      one.two.txt  <== selected",
                "      one.txt",
            ]
        );
    }

    #[gpui::test]
    async fn test_remove_opened_file(cx: &mut gpui::TestAppContext) {
        init_test_with_editor(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/src",
            json!({
                "test": {
                    "first.rs": "// First Rust file",
                    "second.rs": "// Second Rust file",
                    "third.rs": "// Third Rust file",
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/src".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        toggle_expand_dir(&panel, "src/test", cx);
        select_path(&panel, "src/test/first.rs", cx);
        panel.update(cx, |panel, cx| panel.open_file(&Open, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          first.rs  <== selected",
                "          second.rs",
                "          third.rs"
            ]
        );
        ensure_single_file_is_opened(&workspace, "test/first.rs", cx);

        submit_deletion(&panel, cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          second.rs",
                "          third.rs"
            ],
            "Project panel should have no deleted file, no other file is selected in it"
        );
        ensure_no_open_items_and_panes(&workspace, cx);

        select_path(&panel, "src/test/second.rs", cx);
        panel.update(cx, |panel, cx| panel.open_file(&Open, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          second.rs  <== selected",
                "          third.rs"
            ]
        );
        ensure_single_file_is_opened(&workspace, "test/second.rs", cx);

        workspace
            .update(cx, |workspace, cx| {
                let active_items = workspace
                    .panes()
                    .iter()
                    .filter_map(|pane| pane.read(cx).active_item())
                    .collect::<Vec<_>>();
                assert_eq!(active_items.len(), 1);
                let open_editor = active_items
                    .into_iter()
                    .next()
                    .unwrap()
                    .downcast::<Editor>()
                    .expect("Open item should be an editor");
                open_editor.update(cx, |editor, cx| editor.set_text("Another text!", cx));
            })
            .unwrap();
        submit_deletion(&panel, cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &["v src", "    v test", "          third.rs"],
            "Project panel should have no deleted file, with one last file remaining"
        );
        ensure_no_open_items_and_panes(&workspace, cx);
    }

    #[gpui::test]
    async fn test_create_duplicate_items(cx: &mut gpui::TestAppContext) {
        init_test_with_editor(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/src",
            json!({
                "test": {
                    "first.rs": "// First Rust file",
                    "second.rs": "// Second Rust file",
                    "third.rs": "// Third Rust file",
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/src".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        select_path(&panel, "src/", cx);
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                //
                "v src  <== selected",
                "    > test"
            ]
        );
        panel.update(cx, |panel, cx| panel.new_directory(&NewDirectory, cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.filename_editor.read(cx).is_focused(cx));
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                //
                "v src",
                "    > [EDITOR: '']  <== selected",
                "    > test"
            ]
        );
        panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("test", cx));
            assert!(
                panel.confirm_edit(cx).is_none(),
                "Should not allow to confirm on conflicting new directory name"
            )
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                //
                "v src",
                "    > test"
            ],
            "File list should be unchanged after failed folder create confirmation"
        );

        select_path(&panel, "src/test/", cx);
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                //
                "v src",
                "    > test  <== selected"
            ]
        );
        panel.update(cx, |panel, cx| panel.new_file(&NewFile, cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.filename_editor.read(cx).is_focused(cx));
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          [EDITOR: '']  <== selected",
                "          first.rs",
                "          second.rs",
                "          third.rs"
            ]
        );
        panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("first.rs", cx));
            assert!(
                panel.confirm_edit(cx).is_none(),
                "Should not allow to confirm on conflicting new file name"
            )
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          first.rs",
                "          second.rs",
                "          third.rs"
            ],
            "File list should be unchanged after failed file create confirmation"
        );

        select_path(&panel, "src/test/first.rs", cx);
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          first.rs  <== selected",
                "          second.rs",
                "          third.rs"
            ],
        );
        panel.update(cx, |panel, cx| panel.rename(&Rename, cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.filename_editor.read(cx).is_focused(cx));
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          [EDITOR: 'first.rs']  <== selected",
                "          second.rs",
                "          third.rs"
            ]
        );
        panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("second.rs", cx));
            assert!(
                panel.confirm_edit(cx).is_none(),
                "Should not allow to confirm on conflicting file rename"
            )
        });
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          first.rs  <== selected",
                "          second.rs",
                "          third.rs"
            ],
            "File list should be unchanged after failed rename confirmation"
        );
    }

    #[gpui::test]
    async fn test_new_search_in_directory_trigger(cx: &mut gpui::TestAppContext) {
        init_test_with_editor(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/src",
            json!({
                "test": {
                    "first.rs": "// First Rust file",
                    "second.rs": "// Second Rust file",
                    "third.rs": "// Third Rust file",
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/src".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        let new_search_events_count = Arc::new(AtomicUsize::new(0));
        let _subscription = panel.update(cx, |_, cx| {
            let subcription_count = Arc::clone(&new_search_events_count);
            let view = cx.view().clone();
            cx.subscribe(&view, move |_, _, event, _| {
                if matches!(event, Event::NewSearchInDirectory { .. }) {
                    subcription_count.fetch_add(1, atomic::Ordering::SeqCst);
                }
            })
        });

        toggle_expand_dir(&panel, "src/test", cx);
        select_path(&panel, "src/test/first.rs", cx);
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test",
                "          first.rs  <== selected",
                "          second.rs",
                "          third.rs"
            ]
        );
        panel.update(cx, |panel, cx| {
            panel.new_search_in_directory(&NewSearchInDirectory, cx)
        });
        assert_eq!(
            new_search_events_count.load(atomic::Ordering::SeqCst),
            0,
            "Should not trigger new search in directory when called on a file"
        );

        select_path(&panel, "src/test", cx);
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v src",
                "    v test  <== selected",
                "          first.rs",
                "          second.rs",
                "          third.rs"
            ]
        );
        panel.update(cx, |panel, cx| {
            panel.new_search_in_directory(&NewSearchInDirectory, cx)
        });
        assert_eq!(
            new_search_events_count.load(atomic::Ordering::SeqCst),
            1,
            "Should trigger new search in directory when called on a directory"
        );
    }

    #[gpui::test]
    async fn test_collapse_all_entries(cx: &mut gpui::TestAppContext) {
        init_test_with_editor(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree(
            "/project_root",
            json!({
                "dir_1": {
                    "nested_dir": {
                        "file_a.py": "# File contents",
                        "file_b.py": "# File contents",
                        "file_c.py": "# File contents",
                    },
                    "file_1.py": "# File contents",
                    "file_2.py": "# File contents",
                    "file_3.py": "# File contents",
                },
                "dir_2": {
                    "file_1.py": "# File contents",
                    "file_2.py": "# File contents",
                    "file_3.py": "# File contents",
                }
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/project_root".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        panel.update(cx, |panel, cx| {
            panel.collapse_all_entries(&CollapseAllEntries, cx)
        });
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &["v project_root", "    > dir_1", "    > dir_2",]
        );

        // Open dir_1 and make sure nested_dir was collapsed when running collapse_all_entries
        toggle_expand_dir(&panel, "project_root/dir_1", cx);
        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &[
                "v project_root",
                "    v dir_1  <== selected",
                "        > nested_dir",
                "          file_1.py",
                "          file_2.py",
                "          file_3.py",
                "    > dir_2",
            ]
        );
    }

    #[gpui::test]
    async fn test_new_file_move(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor().clone());
        fs.as_fake().insert_tree("/root", json!({})).await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace, cx);
        let panel = workspace
            .update(cx, |workspace, cx| ProjectPanel::new(workspace, cx))
            .unwrap();

        // Make a new buffer with no backing file
        workspace
            .update(cx, |workspace, cx| {
                Editor::new_file(workspace, &Default::default(), cx)
            })
            .unwrap();

        // "Save as"" the buffer, creating a new backing file for it
        let save_task = workspace
            .update(cx, |workspace, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, cx)
            })
            .unwrap();

        cx.executor().run_until_parked();
        cx.simulate_new_path_selection(|_| Some(PathBuf::from("/root/new")));
        save_task.await.unwrap();

        // Rename the file
        select_path(&panel, "root/new", cx);
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &["v root", "      new  <== selected"]
        );
        panel.update(cx, |panel, cx| panel.rename(&Rename, cx));
        panel.update(cx, |panel, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("newer", cx));
        });
        panel.update(cx, |panel, cx| panel.confirm(&Confirm, cx));

        cx.executor().run_until_parked();
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &["v root", "      newer  <== selected"]
        );

        workspace
            .update(cx, |workspace, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, cx)
            })
            .unwrap()
            .await
            .unwrap();

        cx.executor().run_until_parked();
        // assert that saving the file doesn't restore "new"
        assert_eq!(
            visible_entries_as_strings(&panel, 0..10, cx),
            &["v root", "      newer  <== selected"]
        );
    }

    fn toggle_expand_dir(
        panel: &View<ProjectPanel>,
        path: impl AsRef<Path>,
        cx: &mut VisualTestContext,
    ) {
        let path = path.as_ref();
        panel.update(cx, |panel, cx| {
            for worktree in panel.project.read(cx).worktrees().collect::<Vec<_>>() {
                let worktree = worktree.read(cx);
                if let Ok(relative_path) = path.strip_prefix(worktree.root_name()) {
                    let entry_id = worktree.entry_for_path(relative_path).unwrap().id;
                    panel.toggle_expanded(entry_id, cx);
                    return;
                }
            }
            panic!("no worktree for path {:?}", path);
        });
    }

    fn select_path(panel: &View<ProjectPanel>, path: impl AsRef<Path>, cx: &mut VisualTestContext) {
        let path = path.as_ref();
        panel.update(cx, |panel, cx| {
            for worktree in panel.project.read(cx).worktrees().collect::<Vec<_>>() {
                let worktree = worktree.read(cx);
                if let Ok(relative_path) = path.strip_prefix(worktree.root_name()) {
                    let entry_id = worktree.entry_for_path(relative_path).unwrap().id;
                    panel.selection = Some(crate::Selection {
                        worktree_id: worktree.id(),
                        entry_id,
                    });
                    return;
                }
            }
            panic!("no worktree for path {:?}", path);
        });
    }

    fn visible_entries_as_strings(
        panel: &View<ProjectPanel>,
        range: Range<usize>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        let mut result = Vec::new();
        let mut project_entries = HashSet::new();
        let mut has_editor = false;

        panel.update(cx, |panel, cx| {
            panel.for_each_visible_entry(range, cx, |project_entry, details, _| {
                if details.is_editing {
                    assert!(!has_editor, "duplicate editor entry");
                    has_editor = true;
                } else {
                    assert!(
                        project_entries.insert(project_entry),
                        "duplicate project entry {:?} {:?}",
                        project_entry,
                        details
                    );
                }

                let indent = "    ".repeat(details.depth);
                let icon = if details.kind.is_dir() {
                    if details.is_expanded {
                        "v "
                    } else {
                        "> "
                    }
                } else {
                    "  "
                };
                let name = if details.is_editing {
                    format!("[EDITOR: '{}']", details.filename)
                } else if details.is_processing {
                    format!("[PROCESSING: '{}']", details.filename)
                } else {
                    details.filename.clone()
                };
                let selected = if details.is_selected {
                    "  <== selected"
                } else {
                    ""
                };
                result.push(format!("{indent}{icon}{name}{selected}"));
            });
        });

        result
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            init_settings(cx);
            theme::init(theme::LoadThemes::JustBase, cx);
            language::init(cx);
            editor::init_settings(cx);
            crate::init((), cx);
            workspace::init_settings(cx);
            client::init_settings(cx);
            Project::init_settings(cx);

            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings::<ProjectSettings>(cx, |project_settings| {
                    project_settings.file_scan_exclusions = Some(Vec::new());
                });
            });
        });
    }

    fn init_test_with_editor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme::init(theme::LoadThemes::JustBase, cx);
            init_settings(cx);
            language::init(cx);
            editor::init(cx);
            crate::init((), cx);
            workspace::init(app_state.clone(), cx);
            Project::init_settings(cx);
        });
    }

    fn ensure_single_file_is_opened(
        window: &WindowHandle<Workspace>,
        expected_path: &str,
        cx: &mut TestAppContext,
    ) {
        window
            .update(cx, |workspace, cx| {
                let worktrees = workspace.worktrees(cx).collect::<Vec<_>>();
                assert_eq!(worktrees.len(), 1);
                let worktree_id = worktrees[0].read(cx).id();

                let open_project_paths = workspace
                    .panes()
                    .iter()
                    .filter_map(|pane| pane.read(cx).active_item()?.project_path(cx))
                    .collect::<Vec<_>>();
                assert_eq!(
                    open_project_paths,
                    vec![ProjectPath {
                        worktree_id,
                        path: Arc::from(Path::new(expected_path))
                    }],
                    "Should have opened file, selected in project panel"
                );
            })
            .unwrap();
    }

    fn submit_deletion(panel: &View<ProjectPanel>, cx: &mut VisualTestContext) {
        assert!(
            !cx.has_pending_prompt(),
            "Should have no prompts before the deletion"
        );
        panel.update(cx, |panel, cx| panel.delete(&Delete, cx));
        assert!(
            cx.has_pending_prompt(),
            "Should have a prompt after the deletion"
        );
        cx.simulate_prompt_answer(0);
        assert!(
            !cx.has_pending_prompt(),
            "Should have no prompts after prompt was replied to"
        );
        cx.executor().run_until_parked();
    }

    fn ensure_no_open_items_and_panes(
        workspace: &WindowHandle<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        assert!(
            !cx.has_pending_prompt(),
            "Should have no prompts after deletion operation closes the file"
        );
        workspace
            .read_with(cx, |workspace, cx| {
                let open_project_paths = workspace
                    .panes()
                    .iter()
                    .filter_map(|pane| pane.read(cx).active_item()?.project_path(cx))
                    .collect::<Vec<_>>();
                assert!(
                    open_project_paths.is_empty(),
                    "Deleted file's buffer should be closed, but got open files: {open_project_paths:?}"
                );
            })
            .unwrap();
    }
}