mod color_translation;
pub mod connection;
mod modal;
pub mod terminal_element;

use connection::{Event, Terminal, TerminalBuilder, TerminalError};
use dirs::home_dir;
use gpui::{
    actions, elements::*, geometry::vector::vec2f, keymap::Keystroke, AnyViewHandle, AppContext,
    ClipboardItem, Entity, ModelHandle, MutableAppContext, View, ViewContext, ViewHandle,
};
use modal::deploy_modal;

use project::{LocalWorktree, Project, ProjectPath};
use settings::{Settings, WorkingDirectory};
use smallvec::SmallVec;
use std::path::{Path, PathBuf};
use terminal_element::{terminal_layout_context::TerminalLayoutData, TerminalDimensions};
use workspace::{Item, Workspace};

use crate::terminal_element::TerminalEl;

const DEBUG_TERMINAL_WIDTH: f32 = 1000.; //This needs to be wide enough that the prompt can fill the whole space.
const DEBUG_TERMINAL_HEIGHT: f32 = 200.;
const DEBUG_CELL_WIDTH: f32 = 5.;
const DEBUG_LINE_HEIGHT: f32 = 5.;

///Event to transmit the scroll from the element to the view
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollTerminal(pub i32);

actions!(
    terminal,
    [
        Deploy,
        Up,
        Down,
        CtrlC,
        Escape,
        Enter,
        Clear,
        Copy,
        Paste,
        DeployModal
    ]
);

///Initialize and register all of our action handlers
pub fn init(cx: &mut MutableAppContext) {
    //Global binding overrrides
    cx.add_action(ConnectedView::ctrl_c);
    cx.add_action(ConnectedView::up);
    cx.add_action(ConnectedView::down);
    cx.add_action(ConnectedView::escape);
    cx.add_action(ConnectedView::enter);
    //Useful terminal actions
    cx.add_action(ConnectedView::deploy);
    cx.add_action(ConnectedView::copy);
    cx.add_action(ConnectedView::paste);
    cx.add_action(ConnectedView::clear);
    cx.add_action(deploy_modal);
}

//Make terminal view an enum, that can give you views for the error and non-error states
//Take away all the result unwrapping in the current TerminalView by making it 'infallible'
//Bubble up to deploy(_modal)() calls

enum TerminalContent {
    Connected(ViewHandle<ConnectedView>),
    Error(ViewHandle<ErrorView>),
}

impl TerminalContent {
    fn handle(&self) -> AnyViewHandle {
        match self {
            Self::Connected(handle) => handle.into(),
            Self::Error(handle) => handle.into(),
        }
    }
}

pub struct TerminalView {
    modal: bool,
    content: TerminalContent,
}

pub struct ErrorView {
    error: TerminalError,
}

///A terminal view, maintains the PTY's file handles and communicates with the terminal
pub struct ConnectedView {
    terminal: ModelHandle<Terminal>,
    has_new_content: bool,
    //Currently using iTerm bell, show bell emoji in tab until input is received
    has_bell: bool,
    // Only for styling purposes. Doesn't effect behavior
    modal: bool,
}

impl Entity for TerminalView {
    type Event = Event;
}

impl Entity for ConnectedView {
    type Event = Event;
}

impl Entity for ErrorView {
    type Event = Event;
}

impl TerminalView {
    ///Create a new Terminal view. This spawns a task, a thread, and opens the TTY devices
    ///To get the right working directory from a workspace, use: `get_wd_for_workspace()`
    fn new(working_directory: Option<PathBuf>, modal: bool, cx: &mut ViewContext<Self>) -> Self {
        //The details here don't matter, the terminal will be resized on the first layout
        let size_info = TerminalDimensions::new(
            DEBUG_LINE_HEIGHT,
            DEBUG_CELL_WIDTH,
            vec2f(DEBUG_TERMINAL_WIDTH, DEBUG_TERMINAL_HEIGHT),
        );

        let settings = cx.global::<Settings>();
        let shell = settings.terminal_overrides.shell.clone();
        let envs = settings.terminal_overrides.env.clone(); //Should be short and cheap.

        let content = match TerminalBuilder::new(working_directory, shell, envs, size_info) {
            Ok(terminal) => {
                let terminal = cx.add_model(|cx| terminal.subscribe(cx));
                let view = cx.add_view(|cx| ConnectedView::from_terminal(terminal, modal, cx));
                cx.subscribe(&view, |_this, _content, event, cx| cx.emit(event.clone()))
                    .detach();
                TerminalContent::Connected(view)
            }
            Err(error) => {
                let view = cx.add_view(|_| ErrorView {
                    error: error.downcast::<TerminalError>().unwrap(),
                });
                TerminalContent::Error(view)
            }
        };
        cx.focus(content.handle());

        TerminalView { modal, content }
    }

    fn from_terminal(
        terminal: ModelHandle<Terminal>,
        modal: bool,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let connected_view = cx.add_view(|cx| ConnectedView::from_terminal(terminal, modal, cx));
        TerminalView {
            modal,
            content: TerminalContent::Connected(connected_view),
        }
    }
}

impl View for TerminalView {
    fn ui_name() -> &'static str {
        "Terminal"
    }

    fn render(&mut self, cx: &mut gpui::RenderContext<'_, Self>) -> ElementBox {
        let child_view = match &self.content {
            TerminalContent::Connected(connected) => ChildView::new(connected),
            TerminalContent::Error(error) => ChildView::new(error),
        };

        if self.modal {
            let settings = cx.global::<Settings>();
            let container_style = settings.theme.terminal.modal_container;
            child_view.contained().with_style(container_style).boxed()
        } else {
            child_view.boxed()
        }
    }

    fn on_focus(&mut self, cx: &mut ViewContext<Self>) {
        cx.emit(Event::Activate);
        cx.defer(|view, cx| {
            cx.focus(view.content.handle());
        });
    }

    fn keymap_context(&self, _: &gpui::AppContext) -> gpui::keymap::Context {
        let mut context = Self::default_keymap_context();
        if self.modal {
            context.set.insert("ModalTerminal".into());
        }
        context
    }
}

impl ConnectedView {
    fn from_terminal(
        terminal: ModelHandle<Terminal>,
        modal: bool,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&terminal, |this, _, event, cx| match event {
            Event::Wakeup => {
                if cx.is_self_focused() {
                    cx.notify()
                } else {
                    this.has_new_content = true;
                    cx.emit(Event::TitleChanged);
                }
            }
            Event::Bell => {
                this.has_bell = true;
                cx.emit(Event::TitleChanged);
            }
            _ => cx.emit(*event),
        })
        .detach();

        Self {
            terminal,
            has_new_content: true,
            has_bell: false,
            modal,
        }
    }

    fn clear_bel(&mut self, cx: &mut ViewContext<ConnectedView>) {
        self.has_bell = false;
        cx.emit(Event::TitleChanged);
    }

    ///Create a new Terminal in the current working directory or the user's home directory
    fn deploy(workspace: &mut Workspace, _: &Deploy, cx: &mut ViewContext<Workspace>) {
        let working_directory = get_working_directory(workspace, cx);
        let view = cx.add_view(|cx| TerminalView::new(working_directory, false, cx));
        workspace.add_item(Box::new(view), cx);
    }

    fn clear(&mut self, _: &Clear, cx: &mut ViewContext<Self>) {
        self.terminal.read(cx).clear();
    }

    ///Attempt to paste the clipboard into the terminal
    fn copy(&mut self, _: &Copy, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .copy()
            .map(|text| cx.write_to_clipboard(ClipboardItem::new(text)));
    }

    ///Attempt to paste the clipboard into the terminal
    fn paste(&mut self, _: &Paste, cx: &mut ViewContext<Self>) {
        cx.read_from_clipboard().map(|item| {
            self.terminal.read(cx).paste(item.text());
        });
    }

    ///Synthesize the keyboard event corresponding to 'up'
    fn up(&mut self, _: &Up, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .try_keystroke(&Keystroke::parse("up").unwrap());
    }

    ///Synthesize the keyboard event corresponding to 'down'
    fn down(&mut self, _: &Down, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .try_keystroke(&Keystroke::parse("down").unwrap());
    }

    ///Synthesize the keyboard event corresponding to 'ctrl-c'
    fn ctrl_c(&mut self, _: &CtrlC, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .try_keystroke(&Keystroke::parse("ctrl-c").unwrap());
    }

    ///Synthesize the keyboard event corresponding to 'escape'
    fn escape(&mut self, _: &Escape, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .try_keystroke(&Keystroke::parse("escape").unwrap());
    }

    ///Synthesize the keyboard event corresponding to 'enter'
    fn enter(&mut self, _: &Enter, cx: &mut ViewContext<Self>) {
        self.terminal
            .read(cx)
            .try_keystroke(&Keystroke::parse("enter").unwrap());
    }
}

impl View for ConnectedView {
    fn ui_name() -> &'static str {
        "Connected Terminal View"
    }

    fn render(&mut self, cx: &mut gpui::RenderContext<'_, Self>) -> ElementBox {
        let terminal_handle = self.terminal.clone().downgrade();
        TerminalEl::new(cx.handle(), terminal_handle, self.modal)
            .contained()
            .boxed()
    }

    fn on_focus(&mut self, _cx: &mut ViewContext<Self>) {
        self.has_new_content = false;
    }
}

impl View for ErrorView {
    fn ui_name() -> &'static str {
        "Terminal Error"
    }

    fn render(&mut self, cx: &mut gpui::RenderContext<'_, Self>) -> ElementBox {
        let settings = cx.global::<Settings>();
        let style = TerminalLayoutData::make_text_style(cx.font_cache(), settings);

        Label::new(
            format!(
                "Failed to open the terminal. Info: \n{}",
                self.error.to_string()
            ),
            style,
        )
        .aligned()
        .contained()
        .boxed()
    }
}

impl Item for TerminalView {
    fn tab_content(
        &self,
        _detail: Option<usize>,
        tab_theme: &theme::Tab,
        cx: &gpui::AppContext,
    ) -> ElementBox {
        let title = match &self.content {
            TerminalContent::Connected(connected) => {
                connected.read(cx).terminal.read(cx).title.clone()
            }
            TerminalContent::Error(_) => "Terminal".to_string(),
        };

        Flex::row()
            .with_child(
                Label::new(title, tab_theme.label.clone())
                    .aligned()
                    .contained()
                    .boxed(),
            )
            .boxed()
    }

    fn clone_on_split(&self, cx: &mut ViewContext<Self>) -> Option<Self> {
        //From what I can tell, there's no  way to tell the current working
        //Directory of the terminal from outside the shell. There might be
        //solutions to this, but they are non-trivial and require more IPC
        if let TerminalContent::Connected(connected) = &self.content {
            let associated_directory = connected
                .read(cx)
                .terminal
                .read(cx)
                .associated_directory
                .clone();
            Some(TerminalView::new(associated_directory, false, cx))
        } else {
            None
        }
    }

    fn project_path(&self, _cx: &gpui::AppContext) -> Option<ProjectPath> {
        None
    }

    fn project_entry_ids(&self, _cx: &gpui::AppContext) -> SmallVec<[project::ProjectEntryId; 3]> {
        SmallVec::new()
    }

    fn is_singleton(&self, _cx: &gpui::AppContext) -> bool {
        false
    }

    fn set_nav_history(&mut self, _: workspace::ItemNavHistory, _: &mut ViewContext<Self>) {}

    fn can_save(&self, _cx: &gpui::AppContext) -> bool {
        false
    }

    fn save(
        &mut self,
        _project: gpui::ModelHandle<Project>,
        _cx: &mut ViewContext<Self>,
    ) -> gpui::Task<gpui::anyhow::Result<()>> {
        unreachable!("save should not have been called");
    }

    fn save_as(
        &mut self,
        _project: gpui::ModelHandle<Project>,
        _abs_path: std::path::PathBuf,
        _cx: &mut ViewContext<Self>,
    ) -> gpui::Task<gpui::anyhow::Result<()>> {
        unreachable!("save_as should not have been called");
    }

    fn reload(
        &mut self,
        _project: gpui::ModelHandle<Project>,
        _cx: &mut ViewContext<Self>,
    ) -> gpui::Task<gpui::anyhow::Result<()>> {
        gpui::Task::ready(Ok(()))
    }

    fn is_dirty(&self, cx: &gpui::AppContext) -> bool {
        if let TerminalContent::Connected(connected) = &self.content {
            connected.read(cx).has_new_content
        } else {
            false
        }
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        if let TerminalContent::Connected(connected) = &self.content {
            connected.read(cx).has_bell
        } else {
            false
        }
    }

    fn should_update_tab_on_event(event: &Self::Event) -> bool {
        matches!(event, &Event::TitleChanged)
    }

    fn should_close_item_on_event(event: &Self::Event) -> bool {
        matches!(event, &Event::CloseTerminal)
    }

    fn should_activate_item_on_event(event: &Self::Event) -> bool {
        matches!(event, &Event::Activate)
    }
}

///Get's the working directory for the given workspace, respecting the user's settings.
fn get_working_directory(workspace: &Workspace, cx: &AppContext) -> Option<PathBuf> {
    let wd_setting = cx
        .global::<Settings>()
        .terminal_overrides
        .working_directory
        .clone()
        .unwrap_or(WorkingDirectory::CurrentProjectDirectory);
    let res = match wd_setting {
        WorkingDirectory::CurrentProjectDirectory => current_project_directory(workspace, cx),
        WorkingDirectory::FirstProjectDirectory => first_project_directory(workspace, cx),
        WorkingDirectory::AlwaysHome => None,
        WorkingDirectory::Always { directory } => {
            shellexpand::full(&directory) //TODO handle this better
                .ok()
                .map(|dir| Path::new(&dir.to_string()).to_path_buf())
                .filter(|dir| dir.is_dir())
        }
    };
    res.or_else(|| home_dir())
}

///Get's the first project's home directory, or the home directory
fn first_project_directory(workspace: &Workspace, cx: &AppContext) -> Option<PathBuf> {
    workspace
        .worktrees(cx)
        .next()
        .and_then(|worktree_handle| worktree_handle.read(cx).as_local())
        .and_then(get_path_from_wt)
}

///Gets the intuitively correct working directory from the given workspace
///If there is an active entry for this project, returns that entry's worktree root.
///If there's no active entry but there is a worktree, returns that worktrees root.
///If either of these roots are files, or if there are any other query failures,
///  returns the user's home directory
fn current_project_directory(workspace: &Workspace, cx: &AppContext) -> Option<PathBuf> {
    let project = workspace.project().read(cx);

    project
        .active_entry()
        .and_then(|entry_id| project.worktree_for_entry(entry_id, cx))
        .or_else(|| workspace.worktrees(cx).next())
        .and_then(|worktree_handle| worktree_handle.read(cx).as_local())
        .and_then(get_path_from_wt)
}

fn get_path_from_wt(wt: &LocalWorktree) -> Option<PathBuf> {
    wt.root_entry()
        .filter(|re| re.is_dir())
        .map(|_| wt.abs_path().to_path_buf())
}

#[cfg(test)]
mod tests {

    use crate::tests::terminal_test_context::TerminalTestContext;

    use super::*;
    use gpui::TestAppContext;

    use std::path::Path;

    mod terminal_test_context;

    ///Basic integration test, can we get the terminal to show up, execute a command,
    //and produce noticable output?
    #[gpui::test(retries = 5)]
    async fn test_terminal(cx: &mut TestAppContext) {
        let mut cx = TerminalTestContext::new(cx, true);

        cx.execute_and_wait("expr 3 + 4", |content, _cx| content.contains("7"))
            .await;
    }

    ///Working directory calculation tests

    ///No Worktrees in project -> home_dir()
    #[gpui::test]
    async fn no_worktree(cx: &mut TestAppContext) {
        //Setup variables
        let mut cx = TerminalTestContext::new(cx, true);
        let (project, workspace) = cx.blank_workspace().await;
        //Test
        cx.cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure enviroment is as expeted
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_none());

            let res = current_project_directory(workspace, cx);
            assert_eq!(res, None);
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, None);
        });
    }

    ///No active entry, but a worktree, worktree is a file -> home_dir()
    #[gpui::test]
    async fn no_active_entry_worktree_is_file(cx: &mut TestAppContext) {
        //Setup variables

        let mut cx = TerminalTestContext::new(cx, true);
        let (project, workspace) = cx.blank_workspace().await;
        cx.create_file_wt(project.clone(), "/root.txt").await;

        cx.cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure enviroment is as expeted
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = current_project_directory(workspace, cx);
            assert_eq!(res, None);
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, None);
        });
    }

    //No active entry, but a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn no_active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        //Setup variables
        let mut cx = TerminalTestContext::new(cx, true);
        let (project, workspace) = cx.blank_workspace().await;
        let (_wt, _entry) = cx.create_folder_wt(project.clone(), "/root/").await;

        //Test
        cx.cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = current_project_directory(workspace, cx);
            assert_eq!(res, Some((Path::new("/root/")).to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some((Path::new("/root/")).to_path_buf()));
        });
    }

    //Active entry with a work tree, worktree is a file -> home_dir()
    #[gpui::test]
    async fn active_entry_worktree_is_file(cx: &mut TestAppContext) {
        //Setup variables
        let mut cx = TerminalTestContext::new(cx, true);
        let (project, workspace) = cx.blank_workspace().await;
        let (_wt, _entry) = cx.create_folder_wt(project.clone(), "/root1/").await;
        let (wt2, entry2) = cx.create_file_wt(project.clone(), "/root2.txt").await;
        cx.insert_active_entry_for(wt2, entry2, project.clone());

        //Test
        cx.cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = current_project_directory(workspace, cx);
            assert_eq!(res, None);
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some((Path::new("/root1/")).to_path_buf()));
        });
    }

    //Active entry, with a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        //Setup variables
        let mut cx = TerminalTestContext::new(cx, true);
        let (project, workspace) = cx.blank_workspace().await;
        let (_wt, _entry) = cx.create_folder_wt(project.clone(), "/root1/").await;
        let (wt2, entry2) = cx.create_folder_wt(project.clone(), "/root2/").await;
        cx.insert_active_entry_for(wt2, entry2, project.clone());

        //Test
        cx.cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = current_project_directory(workspace, cx);
            assert_eq!(res, Some((Path::new("/root2/")).to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some((Path::new("/root1/")).to_path_buf()));
        });
    }
}
