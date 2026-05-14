use std::cell::RefCell;
use std::cmp::min;
use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, StatefulWidget, Widget,
};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::AppState;
use crate::bazel::{BzlCommand, DetailUpdate, Rule, RunUpdate};

const FOCUSED_VIEW_BORDER_COLOR: Color = Color::Blue;

const FOCUSED_VIEW_SELECTION_HIGHLIGHT_STYLE: Style = Style::new()
    .bg(Color::Gray)
    .fg(Color::Black)
    .add_modifier(Modifier::BOLD);

const UNFOCUSED_VIEW_SELECTION_HIGHLIGHT_STYLE: Style = Style::new()
    .bg(Color::Black)
    .fg(Color::Gray)
    .add_modifier(Modifier::DIM);

#[derive(Clone, Copy, Debug, PartialEq)]
enum ViewId {
    Packages,
    Rules,
}

type Binding = (&'static str, &'static str);

fn render_bindings_line(bindings: &[Binding], area: Rect, buf: &mut Buffer) {
    let spans: Vec<Span> = bindings
        .iter()
        .enumerate()
        .flat_map(|(i, (key, desc))| {
            let mut s = Vec::new();
            if i > 0 {
                s.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            }
            s.push(Span::styled(
                *key,
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
            s.push(Span::raw(format!(": {}", desc)));
            s
        })
        .collect();

    let line = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    line.render(area, buf);
}

pub struct MainView<'a> {
    app: &'a mut AppState,
    packages_view: ListView<'a>,
    rules_view: ListView<'a>,
    active_view_id: Rc<RefCell<ViewId>>,
    modal_open: Rc<RefCell<bool>>,
    modals: Vec<Modal>,
}

enum Modal {
    RuleDetail(RuleContentModal),
    RunOutput(RunOutputModal),
}

pub enum ModalUpdate {
    Detail(DetailUpdate),
    Run(RunUpdate),
}

impl Widget for &mut MainView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(0),
                Constraint::Min(1),
            ])
            .split(area);

        let prefix = self.app.current_prefix();
        let title = format!("bzlui - //{}", prefix[1..].join("/"));

        let content = Paragraph::new(title)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .padding(Padding::horizontal(1)),
            );

        content.render(outer[0], buf);

        self.render_body(outer[2], buf);
    }
}

impl<'a> MainView<'a> {
    pub fn new(app: &'a mut AppState) -> Self {
        let active_view_id = Rc::new(RefCell::new(ViewId::Packages));
        let modal_open = Rc::new(RefCell::new(false));
        let (packages_view, rules_view) =
            Self::build_views(app, active_view_id.clone(), modal_open.clone());

        Self {
            app,
            packages_view,
            rules_view,
            active_view_id,
            modal_open,
            modals: Vec::new(),
        }
    }

    fn build_views(
        app: &AppState,
        active_view_id: Rc<RefCell<ViewId>>,
        modal_open: Rc<RefCell<bool>>,
    ) -> (ListView<'a>, ListView<'a>) {
        let prefix = app.current_prefix();
        let header = format!("//{}", prefix[1..].join("/"));

        let packages = Self::packages_items(app.children.clone());
        let packages_view = ListView::new(
            "1: Packages",
            Some(header),
            packages,
            {
                let active = active_view_id.clone();
                let modal_open = modal_open.clone();
                move || *active.borrow() == ViewId::Packages && !*modal_open.borrow()
            },
            vec![
                ("↑↓/jk/C-u/C-d/PgUp/PgDn", "Navigate"),
                ("Enter", "Select"),
                ("u", "Go to Parent"),
                ("Tab/h/l", "Switch View"),
                ("q", "Quit"),
            ],
            0,
        );

        let rules = Self::rules_items(app.rules.clone());
        let rules_view = ListView::new(
            "2: Rules",
            None,
            rules,
            move || *active_view_id.borrow() == ViewId::Rules && !*modal_open.borrow(),
            vec![
                ("↑↓/jk/C-u/C-d/PgUp/PgDn", "Navigate"),
                ("Enter", "View Rule"),
                ("Tab/h/l", "Switch View"),
                ("q", "Quit"),
            ],
            0,
        );

        (packages_view, rules_view)
    }

    fn focus_view(&mut self, index: usize) {
        match index {
            1 => *self.active_view_id.borrow_mut() = ViewId::Packages,
            2 => *self.active_view_id.borrow_mut() = ViewId::Rules,
            _ => {}
        }
    }

    fn cycle_active_view(&mut self) {
        let mut active_view_ref = self.active_view_id.borrow_mut();
        match *active_view_ref {
            ViewId::Packages => *active_view_ref = ViewId::Rules,
            ViewId::Rules => *active_view_ref = ViewId::Packages,
            _ => {}
        }
    }

    fn refresh_views(&mut self) {
        if self.app.children.is_empty() {
            *self.active_view_id.borrow_mut() = ViewId::Rules;
        } else {
            *self.active_view_id.borrow_mut() = ViewId::Packages;
        }

        let (packages_view, rules_view) = Self::build_views(
            self.app,
            self.active_view_id.clone(),
            self.modal_open.clone(),
        );
        self.packages_view = packages_view;
        self.rules_view = rules_view;
    }

    fn sync_modal_open(&self) {
        *self.modal_open.borrow_mut() = !self.modals.is_empty();
    }

    fn pop_modal(&mut self) {
        self.modals.pop();
        if self.modals.is_empty() {
            *self.active_view_id.borrow_mut() = ViewId::Rules;
        }
        self.sync_modal_open();
    }

    fn select_package(&mut self) {
        self.app
            .enter_selected_package(self.packages_view.selected());
        self.refresh_views();
    }

    fn select_rule(&mut self) {
        let Some(updates_rx) = self.app.spawn_rule_detail(self.rules_view.selected()) else {
            return;
        };
        self.modals
            .push(Modal::RuleDetail(RuleContentModal::new(updates_rx)));
        self.sync_modal_open();
    }

    fn spawn_run(&mut self, command: BzlCommand) {
        let selected = self.rules_view.selected();
        let Some(rule) = self.app.rules.get(selected).cloned() else {
            return;
        };
        let (rx, handle, target) = self.app.spawn_bzl_command(command, &rule);
        let title = format!("{} {}", command.label(), target);
        self.modals
            .push(Modal::RunOutput(RunOutputModal::new(title, rx, handle)));
        self.sync_modal_open();
    }

    pub async fn next_modal_update(&mut self) -> Option<ModalUpdate> {
        match self.modals.last_mut() {
            Some(Modal::RuleDetail(m)) => m.updates_rx.recv().await.map(ModalUpdate::Detail),
            Some(Modal::RunOutput(m)) => m.updates_rx.recv().await.map(ModalUpdate::Run),
            None => std::future::pending().await,
        }
    }

    pub fn apply_modal_update(&mut self, update: ModalUpdate) {
        match (self.modals.last_mut(), update) {
            (Some(Modal::RuleDetail(m)), ModalUpdate::Detail(u)) => m.apply_update(u),
            (Some(Modal::RunOutput(m)), ModalUpdate::Run(u)) => m.apply_update(u),
            _ => {}
        }
    }

    fn go_to_parent_package(&mut self) {
        self.app.go_up();
        self.refresh_views();
    }

    /// Handle the key event, returning true if the event was handled. If this returns false, we
    /// expect the render loop to perform any other handling at the top level (e.g. quitting the
    /// app).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !self.modals.is_empty() {
            return self.handle_modal_key(key);
        }

        match key.code {
            KeyCode::Char('1') | KeyCode::Char('h') => {
                self.focus_view(1);
                return true;
            }
            KeyCode::Char('2') | KeyCode::Char('l') => {
                self.focus_view(2);
                return true;
            }
            KeyCode::Tab => {
                self.cycle_active_view();
                return true;
            }
            _ => {}
        }

        let current = *self.active_view_id.borrow();
        match current {
            ViewId::Packages => {
                if self.packages_view.handle_key(key) {
                    return true;
                }
                match key.code {
                    KeyCode::Enter => {
                        self.select_package();
                        true
                    }
                    KeyCode::Char('u') => {
                        self.go_to_parent_package();
                        true
                    }
                    _ => false,
                }
            }
            ViewId::Rules => {
                if self.rules_view.handle_key(key) {
                    return true;
                }
                match key.code {
                    KeyCode::Enter => {
                        self.select_rule();
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> bool {
        match self.modals.last_mut() {
            None => false,
            Some(Modal::RuleDetail(m)) => {
                let is_runnable = m.is_runnable == Some(true);
                let is_testable = m.is_testable == Some(true);
                match key.code {
                    KeyCode::Char('b') => {
                        self.spawn_run(BzlCommand::Build);
                        true
                    }
                    KeyCode::Char('r') if is_runnable => {
                        self.spawn_run(BzlCommand::Run);
                        true
                    }
                    KeyCode::Char('t') if is_testable => {
                        self.spawn_run(BzlCommand::Test);
                        true
                    }
                    KeyCode::Char('q') => {
                        self.pop_modal();
                        true
                    }
                    _ => false,
                }
            }
            Some(Modal::RunOutput(m)) => match key.code {
                KeyCode::Char('q') => {
                    self.pop_modal();
                    true
                }
                _ => m.handle_key(key),
            },
        }
    }

    fn packages_items(packages: Vec<String>) -> Vec<ListItem<'a>> {
        let last = packages.len().saturating_sub(1);
        packages
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let glyph = if i == last {
                    "└── "
                } else {
                    "├── "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(glyph, Style::default().fg(Color::DarkGray)),
                    Span::raw(p),
                ]))
            })
            .collect()
    }

    fn rules_items(rules: Vec<Rule>) -> Vec<ListItem<'a>> {
        let max_type_len = rules.iter().map(|r| r.rule_type.len()).max().unwrap_or(0);
        rules
            .iter()
            .map(|r| {
                let rule_type = format!("{:>width$}", r.rule_type, width = max_type_len);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        rule_type,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {}", r.name)),
                ]))
            })
            .collect()
    }

    /// Slice out the middle `percent_x`% horizontally and `percent_y`% vertically from a rect area.
    fn centered_rect(percent_x: u16, percent_y: u16, container: Rect) -> Rect {
        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ])
            .split(container);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ])
            .split(popup_layout[1])[1]
    }

    fn render_body(&mut self, area: Rect, buf: &mut Buffer) {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);

        self.packages_view.render(panes[0], buf);
        self.rules_view.render(panes[1], buf);

        if let Some(top) = self.modals.last_mut() {
            let area = Self::centered_rect(80, 80, area);

            // Clear out any text in the area behind the modal before drawing it.
            Clear.render(area, buf);
            match top {
                Modal::RuleDetail(m) => (&*m).render(area, buf),
                Modal::RunOutput(m) => m.render(area, buf),
            }
        }
    }
}

struct ListView<'a> {
    title: &'static str,
    header: Option<String>,
    list_items: Vec<ListItem<'a>>,
    list_state: ListState,
    is_active: Box<dyn Fn() -> bool>,
    bindings: Vec<Binding>,
}

impl Widget for &mut ListView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let focused = (self.is_active)();

        let border_style = if focused {
            Style::default().fg(FOCUSED_VIEW_BORDER_COLOR)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title)
            .title_alignment(Alignment::Center)
            .border_style(border_style)
            .padding(Padding::horizontal(1));

        let inner = block.inner(area);
        block.render(area, buf);

        let (list_area, footer_area) = if let Some(ref header) = self.header {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(inner);

            let header_line = Paragraph::new(Line::from(Span::styled(
                header.as_str(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            header_line.render(sections[0], buf);

            (sections[1], sections[2])
        } else {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(inner);

            (sections[0], sections[1])
        };

        let list = List::new(self.list_items.clone()).highlight_style(if focused {
            FOCUSED_VIEW_SELECTION_HIGHLIGHT_STYLE
        } else {
            UNFOCUSED_VIEW_SELECTION_HIGHLIGHT_STYLE
        });

        StatefulWidget::render(list, list_area, buf, &mut self.list_state);

        if focused {
            render_bindings_line(&self.bindings, footer_area, buf);
        }
    }
}

impl<'a> ListView<'a> {
    fn new<A>(
        title: &'static str,
        header: Option<String>,
        list_items: Vec<ListItem<'a>>,
        is_active: A,
        bindings: Vec<Binding>,
        selected: usize,
    ) -> Self
    where
        A: Fn() -> bool + 'static,
    {
        Self {
            title,
            header,
            list_items,
            list_state: ListState::default().with_selected(Some(selected)),
            is_active: Box::new(is_active),
            bindings,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                true
            }
            KeyCode::PageDown => {
                self.move_down_by(20);
                true
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down_by(20);
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                true
            }
            KeyCode::PageUp => {
                self.move_up_by(20);
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up_by(20);
                true
            }
            _ => false,
        }
    }

    fn selected(&self) -> usize {
        self.list_state.selected().unwrap_or(0)
    }

    fn move_up(&mut self) {
        self.list_state.select_previous();
    }

    fn move_up_by(&mut self, lines: usize) {
        let new_selection = self
            .list_state
            .selected()
            .unwrap_or_default()
            .saturating_sub(lines);
        self.list_state.select(Some(new_selection));
    }

    fn move_down(&mut self) {
        self.list_state.select_next();
    }

    fn move_down_by(&mut self, lines: usize) {
        let new_selection = min(
            self.list_state.selected().unwrap_or_default() + lines,
            self.list_items.len() - 1,
        );
        self.list_state.select(Some(new_selection));
    }
}

enum RuleText {
    Loading,
    Loaded(String),
    Error(String),
}

struct RuleContentModal {
    text: RuleText,
    is_runnable: Option<bool>,
    is_testable: Option<bool>,
    updates_rx: mpsc::UnboundedReceiver<DetailUpdate>,
    bindings: Vec<Binding>,
}

impl Widget for &RuleContentModal {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FOCUSED_VIEW_BORDER_COLOR))
            .title("Rule Detail")
            .title_alignment(Alignment::Center)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, buf);

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);

        match &self.text {
            RuleText::Loading => Paragraph::new("Loading…")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray))
                .render(sections[0], buf),
            RuleText::Loaded(s) => Paragraph::new(s.clone()).render(sections[0], buf),
            RuleText::Error(e) => Paragraph::new(e.clone())
                .style(Style::default().fg(Color::Red))
                .render(sections[0], buf),
        }
        render_bindings_line(&self.bindings, sections[1], buf);
    }
}

impl RuleContentModal {
    fn new(updates_rx: mpsc::UnboundedReceiver<DetailUpdate>) -> Self {
        let mut modal = Self {
            text: RuleText::Loading,
            is_runnable: None,
            is_testable: None,
            updates_rx,
            bindings: Vec::new(),
        };
        modal.rebuild_bindings();
        modal
    }

    fn apply_update(&mut self, update: DetailUpdate) {
        match update {
            DetailUpdate::Text(Ok(s)) => self.text = RuleText::Loaded(s),
            DetailUpdate::Text(Err(e)) => self.text = RuleText::Error(e),
            DetailUpdate::Runnable(v) => self.is_runnable = Some(v),
            DetailUpdate::Testable(v) => self.is_testable = Some(v),
        }
        self.rebuild_bindings();
    }

    fn rebuild_bindings(&mut self) {
        let mut bindings: Vec<Binding> = vec![("b", "Build")];
        if self.is_runnable == Some(true) {
            bindings.push(("r", "Run"));
        }
        if self.is_testable == Some(true) {
            bindings.push(("t", "Test"));
        }
        bindings.push(("q", "Close"));
        self.bindings = bindings;
    }
}

enum RunStatus {
    Running,
    Exited(Option<i32>),
    SpawnFailed(String),
}

struct RunOutputModal {
    title: String,
    lines: Vec<String>,
    status: RunStatus,
    updates_rx: mpsc::UnboundedReceiver<RunUpdate>,
    _task: JoinHandle<()>,
    scroll: u16,
    follow_tail: bool,
    bindings: Vec<Binding>,
}

impl RunOutputModal {
    fn new(
        title: String,
        updates_rx: mpsc::UnboundedReceiver<RunUpdate>,
        task: JoinHandle<()>,
    ) -> Self {
        Self {
            title,
            lines: Vec::new(),
            status: RunStatus::Running,
            updates_rx,
            _task: task,
            scroll: 0,
            follow_tail: true,
            bindings: vec![
                ("↑↓/jk/C-u/C-d/PgUp/PgDn", "Scroll"),
                ("G", "Tail"),
                ("q", "Close"),
            ],
        }
    }

    fn apply_update(&mut self, update: RunUpdate) {
        match update {
            RunUpdate::Line(s) => self.lines.push(s),
            RunUpdate::Exited(code) => self.status = RunStatus::Exited(code),
            RunUpdate::SpawnFailed(e) => self.status = RunStatus::SpawnFailed(e),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let max_scroll = self.lines.len().saturating_sub(1) as u16;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1).min(max_scroll);
                self.follow_tail = false;
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                self.follow_tail = false;
                true
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(20).min(max_scroll);
                self.follow_tail = false;
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(20);
                self.follow_tail = false;
                true
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_add(20).min(max_scroll);
                self.follow_tail = false;
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_sub(20);
                self.follow_tail = false;
                true
            }
            KeyCode::Char('G') => {
                self.follow_tail = true;
                true
            }
            _ => false,
        }
    }
}

impl Widget for &mut RunOutputModal {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FOCUSED_VIEW_BORDER_COLOR))
            .title(self.title.as_str())
            .title_alignment(Alignment::Center)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        block.render(area, buf);

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);

        let body_area = sections[0];
        let footer_area = sections[1];

        let mut text_lines: Vec<Line> = self.lines.iter().map(|l| Line::raw(l.as_str())).collect();
        match &self.status {
            RunStatus::Running => {}
            RunStatus::Exited(Some(0)) => text_lines.push(Line::styled(
                "--- exited (0) ---",
                Style::default().fg(Color::Green),
            )),
            RunStatus::Exited(Some(n)) => text_lines.push(Line::styled(
                format!("--- exited ({n}) ---"),
                Style::default().fg(Color::Red),
            )),
            RunStatus::Exited(None) => text_lines.push(Line::styled(
                "--- exited ---",
                Style::default().fg(Color::Yellow),
            )),
            RunStatus::SpawnFailed(e) => text_lines.push(Line::styled(
                format!("--- failed to spawn: {e} ---"),
                Style::default().fg(Color::Red),
            )),
        }

        let total = text_lines.len() as u16;
        let height = body_area.height;
        let max_scroll = total.saturating_sub(height);
        if self.follow_tail {
            self.scroll = max_scroll;
        } else if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        Paragraph::new(text_lines)
            .scroll((self.scroll, 0))
            .render(body_area, buf);

        render_bindings_line(&self.bindings, footer_area, buf);
    }
}
