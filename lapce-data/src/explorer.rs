use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use druid::Command;
use druid::EventCtx;
use druid::ExtEventSink;
use druid::{Target, WidgetId};

use lapce_core::cursor::CursorMode;
use lapce_core::selection::Selection;
use lapce_rpc::file::FileNodeItem;
use lapce_rpc::proxy::ReadDirResponse;
use xi_rope::Rope;

use crate::data::LapceMainSplitData;
use crate::data::LapceWorkspace;
use crate::document::LocalBufferKind;
use crate::proxy::LapceProxy;

use crate::{command::LapceUICommand, command::LAPCE_UI_COMMAND};

#[derive(Clone)]
pub enum Naming {
    /// Renaming an existing file
    Renaming {
        /// The index into the file list of the file being renamed
        list_index: usize,
        /// Indentation level
        indent_level: usize,
    },
    /// Naming a file that has yet to be created
    Naming {
        /// The index that the file being created should appear at
        /// Note that when naming, it is not yet actually created.
        list_index: usize,
        /// Indentation level
        indent_level: usize,
        /// If true, then we are creating a directory
        /// If false, then we are creating a file
        is_dir: bool,
        /// The folder that the file/directory is being created within
        base_path: PathBuf,
    },
}
impl Naming {
    pub fn list_index(&self) -> usize {
        match self {
            Naming::Renaming { list_index, .. }
            | Naming::Naming { list_index, .. } => *list_index,
        }
    }
}

#[derive(Clone)]
pub struct FileExplorerData {
    pub tab_id: WidgetId,
    pub widget_id: WidgetId,
    pub workspace: Option<FileNodeItem>,
    pub active_selected: Option<PathBuf>,
    /// The status of renaming/naming a file/directory
    pub naming: Option<Naming>,
    /// The id of the editor (in `main_split.editors`) for renaming
    pub renaming_editor_view_id: WidgetId,
}

impl FileExplorerData {
    pub fn new(
        tab_id: WidgetId,
        workspace: LapceWorkspace,
        proxy: Arc<LapceProxy>,
        event_sink: ExtEventSink,
    ) -> Self {
        let mut items = Vec::new();
        let widget_id = WidgetId::next();
        if let Some(path) = workspace.path.as_ref() {
            items.push(FileNodeItem {
                path_buf: path.clone(),
                is_dir: true,
                read: false,
                open: false,
                children: HashMap::new(),
                children_open_count: 0,
            });
            let path = path.clone();
            std::thread::spawn(move || {
                Self::read_dir(&path, true, tab_id, &proxy, event_sink);
            });
        }
        Self {
            tab_id,
            widget_id,
            workspace: workspace.path.as_ref().map(|p| FileNodeItem {
                path_buf: p.clone(),
                is_dir: true,
                read: false,
                open: false,
                children: HashMap::new(),
                children_open_count: 0,
            }),
            active_selected: None,
            naming: None,
            renaming_editor_view_id: WidgetId::next(),
        }
    }

    pub fn update_node_count(&mut self, path: &Path) -> Option<()> {
        let node = self.get_node_mut(path)?;
        if node.is_dir {
            if node.open {
                node.children_open_count = node
                    .children
                    .iter()
                    .map(|(_, item)| item.children_open_count + 1)
                    .sum::<usize>();
            } else {
                node.children_open_count = 0;
            }
        }
        None
    }

    pub fn node_tree(&mut self, path: &Path) -> Option<Vec<PathBuf>> {
        let root = &self.workspace.as_ref()?.path_buf;
        let path = path.strip_prefix(root).ok()?;
        Some(
            path.ancestors()
                .map(|p| root.join(p))
                .collect::<Vec<PathBuf>>(),
        )
    }

    /// Get the node by its index into the file list
    /// Returns the node and its indentation level
    pub fn get_node_by_index(&self, index: usize) -> Option<(usize, &FileNodeItem)> {
        let (_, node) = get_item_children(0, index, 0, self.workspace.as_ref()?);
        node
    }

    /// Get the node by its index into the file list
    /// Returns the node and its indentation level
    pub fn get_node_by_index_mut(
        &mut self,
        index: usize,
    ) -> Option<(usize, &mut FileNodeItem)> {
        let (_, node) = get_item_children_mut(0, index, 0, self.workspace.as_mut()?);
        node
    }

    pub fn get_node_mut(&mut self, path: &Path) -> Option<&mut FileNodeItem> {
        let mut node = self.workspace.as_mut()?;
        if node.path_buf == path {
            return Some(node);
        }
        let root = node.path_buf.clone();
        let path = path.strip_prefix(&root).ok()?;
        for path in path.ancestors().collect::<Vec<&Path>>().iter().rev() {
            if path.to_str()?.is_empty() {
                continue;
            }
            node = node.children.get_mut(&root.join(path))?;
        }
        Some(node)
    }

    pub fn update_children(
        &mut self,
        path: &Path,
        children: HashMap<PathBuf, FileNodeItem>,
        expand: bool,
    ) -> Option<()> {
        // Ignore updates while naming a file
        if self.naming.is_some() {
            return None;
        }

        let node = self.workspace.as_mut()?.get_file_node_mut(path)?;

        let removed_paths: Vec<PathBuf> = node
            .children
            .keys()
            .filter(|p| !children.contains_key(*p))
            .map(PathBuf::from)
            .collect();
        for path in removed_paths {
            node.children.remove(&path);
        }

        for (path, child) in children.into_iter() {
            if !node.children.contains_key(&path) {
                node.children.insert(child.path_buf.clone(), child);
            }
        }

        node.read = true;
        if expand {
            node.open = true;
        }

        for p in path.ancestors() {
            self.update_node_count(p);
        }

        Some(())
    }

    pub fn read_dir(
        path: &Path,
        expand: bool,
        tab_id: WidgetId,
        proxy: &LapceProxy,
        event_sink: ExtEventSink,
    ) {
        FileExplorerData::read_dir_cb::<fn()>(
            path, expand, tab_id, proxy, event_sink, None,
        )
    }

    pub fn read_dir_cb<F: FnOnce() + Send + 'static>(
        path: &Path,
        expand: bool,
        tab_id: WidgetId,
        proxy: &LapceProxy,
        event_sink: ExtEventSink,
        mut on_finished: Option<F>,
    ) {
        let path = PathBuf::from(path);
        let local_path = path.clone();
        proxy.read_dir(
            &local_path,
            Box::new(move |result| {
                if let Ok(res) = result {
                    let path = path.clone();
                    let resp: Result<ReadDirResponse, serde_json::Error> =
                        serde_json::from_value(res);
                    if let Ok(resp) = resp {
                        let _ = event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateExplorerItems(
                                path, resp.items, expand,
                            ),
                            Target::Widget(tab_id),
                        );

                        if let Some(on_finished) = on_finished.take() {
                            on_finished();
                        }
                    }
                }
            }),
        );
    }

    /// Stop naming the file/directory, discarding any changes
    pub fn cancel_naming(&mut self) {
        self.naming = None;
    }

    /// Apply the current naming/renaming text (if it is nonempty and not the same as before)
    /// Also stops the naming.
    pub fn apply_naming(
        &mut self,
        ctx: &mut EventCtx,
        main_split: &LapceMainSplitData,
    ) {
        let naming = if let Some(naming) = &self.naming {
            naming
        } else {
            return;
        };

        // Get the text in the input
        let doc = main_split
            .local_docs
            .get(&LocalBufferKind::PathName)
            .unwrap();
        let target_name = doc.buffer().text().to_string();
        // If the name is empty, then we just ignore it
        if target_name.is_empty() {
            self.cancel_naming();
            return;
        }

        match naming {
            Naming::Renaming { list_index, .. } => {
                let renaming =
                    if let Some((_, node)) = self.get_node_by_index(*list_index) {
                        &node.path_buf
                    } else {
                        // There was either nothing we were renaming, or the index disappeared
                        return;
                    };

                let target_path = renaming.with_file_name(target_name);

                // If it is the same, then we don't bother renaming it
                if &target_path == renaming {
                    return;
                }

                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::RenamePath {
                        from: renaming.clone(),
                        to: target_path,
                    },
                    Target::Auto,
                ));
            }
            Naming::Naming {
                is_dir, base_path, ..
            } => {
                let mut path = base_path.clone();
                path.push(target_name);

                let cmd = if *is_dir {
                    LapceUICommand::CreateDirectory { path }
                } else {
                    LapceUICommand::CreateFileOpen { path }
                };
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    cmd,
                    Target::Auto,
                ));
            }
        }

        self.cancel_naming();
    }

    pub fn start_naming(
        &mut self,
        ctx: &mut EventCtx,
        main_split: &mut LapceMainSplitData,
        list_index: usize,
        indent_level: usize,
        is_dir: bool,
        base_path: PathBuf,
    ) {
        self.cancel_naming();
        self.naming = Some(Naming::Naming {
            list_index,
            indent_level,
            is_dir,
            base_path,
        });

        // Clear the text of the input
        let doc = main_split
            .local_docs
            .get_mut(&LocalBufferKind::PathName)
            .unwrap();
        Arc::make_mut(doc).reload(Rope::from(String::new()), true);

        // Make sure the cursor is at the right position
        let editor = main_split
            .editors
            .get_mut(&self.renaming_editor_view_id)
            .unwrap();
        Arc::make_mut(editor).cursor.mode = CursorMode::Insert(Selection::caret(0));

        // Focus on the input
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::Focus,
            Target::Widget(editor.view_id),
        ));
    }

    /// Show the renaming input for the given file at the index
    /// Requires `main_split` for getting the input to set its content
    /// Requires `ctx` to switch focus to the input
    pub fn start_renaming(
        &mut self,
        ctx: &mut EventCtx,
        main_split: &mut LapceMainSplitData,
        list_index: usize,
        indent_level: usize,
        text: String,
    ) {
        self.cancel_naming();
        self.naming = Some(Naming::Renaming {
            list_index,
            indent_level,
        });

        // Set the text of the input
        let doc = main_split
            .local_docs
            .get_mut(&LocalBufferKind::PathName)
            .unwrap();
        Arc::make_mut(doc).reload(Rope::from(text), true);

        // TODO: We could provide a configuration option to only select the filename at first,
        // which would fit a common case of just wanting to change the filename and not the ext
        // (or that could be the default)

        // Select all of the text, allowing them to quickly completely change the name if they wish
        let editor = main_split
            .editors
            .get_mut(&self.renaming_editor_view_id)
            .unwrap();
        let offset = doc.buffer().line_end_offset(0, true);
        Arc::make_mut(editor).cursor.mode =
            CursorMode::Insert(Selection::region(0, offset));

        // Focus on the input
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::Focus,
            Target::Widget(editor.view_id),
        ));
    }
}

/// Returns (current index, Option<(indentation level of item, item)>)
pub fn get_item_children(
    i: usize,
    index: usize,
    indent: usize,
    item: &FileNodeItem,
) -> (usize, Option<(usize, &FileNodeItem)>) {
    if i == index {
        return (i, Some((indent, item)));
    }
    let mut i = i;
    if item.open {
        for child in item.sorted_children() {
            let count = child.children_open_count;
            if i + count + 1 >= index {
                let (new_index, node) =
                    get_item_children(i + 1, index, indent + 1, child);
                if new_index == index {
                    return (new_index, node);
                }
            }
            i += count + 1;
        }
    }
    (i, None)
}

pub fn get_item_children_mut(
    i: usize,
    index: usize,
    indent: usize,
    item: &mut FileNodeItem,
) -> (usize, Option<(usize, &mut FileNodeItem)>) {
    if i == index {
        return (i, Some((indent, item)));
    }
    let mut i = i;
    if item.open {
        for child in item.sorted_children_mut() {
            let count = child.children_open_count;
            if i + count + 1 >= index {
                let (new_index, node) =
                    get_item_children_mut(i + 1, index, indent + 1, child);
                if new_index == index {
                    return (new_index, node);
                }
            }
            i += count + 1;
        }
    }
    (i, None)
}
