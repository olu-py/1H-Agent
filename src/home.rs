use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    commands::AgentMode,
    config::ProviderConfig,
    input::{InputBuffer, input_cursor_viewport},
    storage::SessionSummary,
    ui_theme::{UiTheme, VisualRole},
};

pub(crate) const RECENT_SESSION_LIMIT: usize = 5;
const WORDMARK_WIDTH: u16 = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeFocus {
    Composer,
    Recents,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeAction {
    StartNew(String),
    Resume(String),
    Quit,
}

#[derive(Clone, Debug)]
pub(crate) struct HomeSelection {
    pub provider: ProviderConfig,
    pub mode: AgentMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeMenuKind {
    Provider,
    Model,
}

#[derive(Clone, Copy, Debug)]
struct HomeMenuState {
    kind: HomeMenuKind,
    selected: usize,
    rect: Option<Rect>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HomeEventOutcome {
    pub action: Option<HomeAction>,
    pub redraw: bool,
}

pub(crate) struct HomeState {
    input: InputBuffer,
    recent_sessions: Vec<SessionSummary>,
    focus: HomeFocus,
    selected_session: usize,
    workspace_label: String,
    provider: ProviderConfig,
    providers: Vec<ProviderConfig>,
    mode: AgentMode,
    menu: Option<HomeMenuState>,
    loading: bool,
    input_rect: Option<Rect>,
    mode_rect: Option<Rect>,
    provider_rect: Option<Rect>,
    model_rect: Option<Rect>,
    session_rects: Vec<(Rect, String)>,
}

impl HomeState {
    pub(crate) fn new(
        workspace: &Path,
        provider: ProviderConfig,
        providers: Vec<ProviderConfig>,
        recent_sessions: Vec<SessionSummary>,
    ) -> Self {
        let workspace_label = workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace.to_string_lossy().into_owned());
        let mut unique_providers = Vec::with_capacity(providers.len().saturating_add(1));
        for candidate in providers {
            if !unique_providers
                .iter()
                .any(|existing: &ProviderConfig| existing.preset == candidate.preset)
            {
                unique_providers.push(candidate);
            }
        }
        if let Some(active) = unique_providers
            .iter_mut()
            .find(|candidate| candidate.preset == provider.preset)
        {
            *active = provider.clone();
        } else {
            unique_providers.insert(0, provider.clone());
        }
        Self {
            input: InputBuffer::new(),
            recent_sessions,
            focus: HomeFocus::Composer,
            selected_session: 0,
            workspace_label,
            provider,
            providers: unique_providers,
            mode: AgentMode::Build,
            menu: None,
            loading: false,
            input_rect: None,
            mode_rect: None,
            provider_rect: None,
            model_rect: None,
            session_rects: Vec::new(),
        }
    }

    pub(crate) fn selection(&self) -> HomeSelection {
        HomeSelection {
            provider: self.provider.clone(),
            mode: self.mode,
        }
    }

    pub(crate) fn set_loading(&mut self) {
        self.loading = true;
    }

    pub(crate) fn handle_event(&mut self, event: Event) -> HomeEventOutcome {
        if let Event::Paste(text) = event {
            if self.loading || self.menu.is_some() {
                return HomeEventOutcome::default();
            }
            self.focus = HomeFocus::Composer;
            self.input.insert_str(&text);
            return redraw();
        }
        if let Event::Resize(_, _) = event {
            return redraw();
        }
        if let Event::Mouse(mouse) = event {
            if self.loading || mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                return HomeEventOutcome::default();
            }
            if self.menu.is_some() {
                return self.handle_menu_mouse(mouse.column, mouse.row);
            }
            if self
                .mode_rect
                .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
            {
                self.mode = next_mode(self.mode);
                return redraw();
            }
            if self
                .provider_rect
                .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
            {
                self.open_menu(HomeMenuKind::Provider);
                return redraw();
            }
            if self
                .model_rect
                .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
            {
                self.open_menu(HomeMenuKind::Model);
                return redraw();
            }
            if self
                .input_rect
                .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
            {
                self.focus = HomeFocus::Composer;
                return redraw();
            }
            if let Some((_, session_id)) = self
                .session_rects
                .iter()
                .find(|(rect, _)| point_in_rect(mouse.column, mouse.row, *rect))
            {
                return action(HomeAction::Resume(session_id.clone()));
            }
            return HomeEventOutcome::default();
        }
        let Event::Key(key) = event else {
            return HomeEventOutcome::default();
        };
        if key.kind != KeyEventKind::Press || self.loading {
            return HomeEventOutcome::default();
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return action(HomeAction::Quit);
        }
        if self.menu.is_some() {
            return self.handle_menu_key(key.code);
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && !self.recent_sessions.is_empty() {
            self.focus = match self.focus {
                HomeFocus::Composer => HomeFocus::Recents,
                HomeFocus::Recents => HomeFocus::Composer,
            };
            return redraw();
        }
        match self.focus {
            HomeFocus::Composer => self.handle_composer_key(key.code, key.modifiers),
            HomeFocus::Recents => self.handle_recents_key(key.code),
        }
    }

    fn handle_composer_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> HomeEventOutcome {
        match code {
            KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => self.input.insert('\n'),
            KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => {
                let prompt = self.input.as_str().trim();
                if prompt.is_empty() {
                    return HomeEventOutcome::default();
                }
                return action(HomeAction::StartNew(prompt.to_owned()));
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if modifiers.contains(KeyModifiers::SHIFT) => self.input.select_left(),
            KeyCode::Right if modifiers.contains(KeyModifiers::SHIFT) => self.input.select_right(),
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.select_all()
            }
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Up => self.input.move_up(),
            KeyCode::Down => self.input.move_down(),
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.delete_word_left()
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => self.input.clear(),
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert(character)
            }
            _ => return HomeEventOutcome::default(),
        }
        redraw()
    }

    fn handle_recents_key(&mut self, code: KeyCode) -> HomeEventOutcome {
        match code {
            KeyCode::Up => {
                self.selected_session = self.selected_session.saturating_sub(1);
                redraw()
            }
            KeyCode::Down => {
                self.selected_session =
                    (self.selected_session + 1).min(self.recent_sessions.len().saturating_sub(1));
                redraw()
            }
            KeyCode::Enter => self
                .recent_sessions
                .get(self.selected_session)
                .map(|session| action(HomeAction::Resume(session.id.clone())))
                .unwrap_or_default(),
            KeyCode::Esc => {
                self.focus = HomeFocus::Composer;
                redraw()
            }
            _ => HomeEventOutcome::default(),
        }
    }

    fn provider_label(&self) -> &'static str {
        self.provider.preset.label()
    }

    fn model_choices(&self) -> Vec<String> {
        let mut choices = self
            .provider
            .preset
            .selectable_models()
            .iter()
            .map(|model| (*model).to_owned())
            .collect::<Vec<_>>();
        if choices.is_empty() {
            choices.push(self.provider.model.clone());
        } else if !choices.iter().any(|model| model == &self.provider.model) {
            choices.insert(0, self.provider.model.clone());
        }
        choices
    }

    fn open_menu(&mut self, kind: HomeMenuKind) {
        let selected = match kind {
            HomeMenuKind::Provider => self
                .providers
                .iter()
                .position(|provider| provider.preset == self.provider.preset)
                .unwrap_or(0),
            HomeMenuKind::Model => self
                .model_choices()
                .iter()
                .position(|model| model == &self.provider.model)
                .unwrap_or(0),
        };
        self.menu = Some(HomeMenuState {
            kind,
            selected,
            rect: None,
        });
    }

    fn handle_menu_key(&mut self, code: KeyCode) -> HomeEventOutcome {
        let Some(menu) = self.menu else {
            return HomeEventOutcome::default();
        };
        let len = match menu.kind {
            HomeMenuKind::Provider => self.providers.len(),
            HomeMenuKind::Model => self.model_choices().len(),
        };
        match code {
            KeyCode::Esc => self.menu = None,
            KeyCode::Up if len > 0 => {
                if let Some(active) = self.menu.as_mut() {
                    active.selected = (active.selected + len - 1) % len;
                }
            }
            KeyCode::Down if len > 0 => {
                if let Some(active) = self.menu.as_mut() {
                    active.selected = (active.selected + 1) % len;
                }
            }
            KeyCode::Enter if len > 0 => {
                let kind = menu.kind;
                let selected = menu.selected.min(len - 1);
                self.apply_menu_choice(kind, selected);
                self.menu = None;
            }
            _ => return HomeEventOutcome::default(),
        }
        redraw()
    }

    fn handle_menu_mouse(&mut self, column: u16, row: u16) -> HomeEventOutcome {
        let Some(menu) = self.menu else {
            return HomeEventOutcome::default();
        };
        let Some(rect) = menu.rect else {
            self.menu = None;
            return redraw();
        };
        if !point_in_rect(column, row, rect) {
            self.menu = None;
            return redraw();
        }
        let inner = Block::bordered().inner(rect);
        if !point_in_rect(column, row, inner) {
            return HomeEventOutcome::default();
        }
        let len = match menu.kind {
            HomeMenuKind::Provider => self.providers.len(),
            HomeMenuKind::Model => self.model_choices().len(),
        };
        let visible = inner.height as usize;
        let scroll = menu.selected.saturating_sub(visible.saturating_sub(1));
        let selected = scroll.saturating_add(row.saturating_sub(inner.y) as usize);
        if selected < len {
            self.apply_menu_choice(menu.kind, selected);
        }
        self.menu = None;
        redraw()
    }

    fn apply_menu_choice(&mut self, kind: HomeMenuKind, selected: usize) {
        match kind {
            HomeMenuKind::Provider => {
                if let Some(provider) = self.providers.get(selected).cloned() {
                    self.provider = provider;
                }
            }
            HomeMenuKind::Model => {
                if let Some(model) = self.model_choices().get(selected).cloned() {
                    self.provider.model = model;
                    self.provider.normalize_thinking();
                    if let Some(saved) = self
                        .providers
                        .iter_mut()
                        .find(|provider| provider.preset == self.provider.preset)
                    {
                        *saved = self.provider.clone();
                    }
                }
            }
        }
    }
}

fn next_mode(mode: AgentMode) -> AgentMode {
    match mode {
        AgentMode::Build => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Explore,
        AgentMode::Explore => AgentMode::Cluster,
        AgentMode::Cluster => AgentMode::Build,
    }
}

fn redraw() -> HomeEventOutcome {
    HomeEventOutcome {
        action: None,
        redraw: true,
    }
}

fn action(action: HomeAction) -> HomeEventOutcome {
    HomeEventOutcome {
        action: Some(action),
        redraw: false,
    }
}

pub(crate) fn draw(frame: &mut Frame<'_>, state: &mut HomeState) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    state.input_rect = None;
    state.mode_rect = None;
    state.provider_rect = None;
    state.model_rect = None;
    state.session_rects.clear();
    if let Some(menu) = state.menu.as_mut() {
        menu.rect = None;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = UiTheme::default();
    let footer_height = 1.min(area.height);
    let topbar_height = if area.height >= 22 { 2 } else { 0 };
    if topbar_height > 0 {
        draw_topbar(
            frame,
            Rect::new(area.x, area.y, area.width, topbar_height),
            state,
            &theme,
        );
    }
    let body = Rect::new(
        area.x,
        area.y.saturating_add(topbar_height),
        area.width,
        area.height
            .saturating_sub(footer_height)
            .saturating_sub(topbar_height),
    );
    let content_width = area.width.saturating_sub(4).clamp(1, 104);
    let content_x = area.x + area.width.saturating_sub(content_width) / 2;

    if body.height >= 19 && content_width >= WORDMARK_WIDTH {
        let spacious = body.height >= 25;
        let hero_height = if spacious { 11 } else { 9 };
        let hero_gap = if spacious { 2 } else { 1 };
        let recents_gap = if spacious { 2 } else { 1 };
        let fixed_height = hero_height + hero_gap + 5 + recents_gap + 1;
        let recent_rows = usize::from(body.height.saturating_sub(fixed_height) / 2)
            .min(state.recent_sessions.len())
            .min(RECENT_SESSION_LIMIT);
        let desired_height = fixed_height.saturating_add((recent_rows as u16).saturating_mul(2));
        let mut y = body.y + body.height.saturating_sub(desired_height) / 2;
        draw_hero(
            frame,
            Rect::new(content_x, y, content_width, hero_height),
            state,
            &theme,
            spacious,
        );
        y = y.saturating_add(hero_height).saturating_add(hero_gap);
        let input = Rect::new(content_x, y, content_width, 5);
        draw_input(frame, input, state, &theme);
        y = input.bottom().saturating_add(recents_gap);
        draw_recents(
            frame,
            Rect::new(
                content_x,
                y,
                content_width,
                1u16.saturating_add((recent_rows as u16).saturating_mul(2)),
            ),
            state,
            &theme,
            true,
        );
    } else if body.height >= 9 {
        let recent_rows = usize::from(body.height.saturating_sub(9))
            .min(state.recent_sessions.len())
            .min(RECENT_SESSION_LIMIT);
        let desired_height = 9u16.saturating_add(recent_rows as u16);
        let mut y = body.y + body.height.saturating_sub(desired_height) / 2;
        draw_compact_brand(
            frame,
            Rect::new(content_x, y, content_width, 2),
            state,
            &theme,
        );
        y = y.saturating_add(3);
        draw_input(
            frame,
            Rect::new(content_x, y, content_width, 5),
            state,
            &theme,
        );
        y = y.saturating_add(5);
        draw_recents(
            frame,
            Rect::new(
                content_x,
                y,
                content_width,
                1u16.saturating_add(recent_rows as u16),
            ),
            state,
            &theme,
            false,
        );
    } else if body.height >= 7 {
        let y = body.y + body.height.saturating_sub(7) / 2;
        frame.render_widget(
            Paragraph::new("1H-Agent").style(theme.strong(VisualRole::Accent)),
            Rect::new(content_x, y, content_width, 1),
        );
        draw_input(
            frame,
            Rect::new(content_x, y.saturating_add(2), content_width, 5),
            state,
            &theme,
        );
    } else if body.height > 0 {
        draw_input(
            frame,
            Rect::new(content_x, body.y, content_width, body.height),
            state,
            &theme,
        );
    }

    let footer = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        footer_height,
    );
    draw_footer(frame, footer, state, &theme);
    draw_menu(frame, area, footer, state, &theme);
}

fn draw_topbar(frame: &mut Frame<'_>, area: Rect, state: &HomeState, theme: &UiTheme) {
    if area.height == 0 {
        return;
    }
    let left = if area.width >= 64 {
        format!(
            " H  1H-Agent / protium  |  workspace  {}",
            state.workspace_label
        )
    } else {
        " H  1H-Agent".to_owned()
    };
    let right = "* LOCAL RUNTIME  READY";
    let text = if area.width >= 48 {
        join_ends(&left, right, area.width as usize)
    } else {
        fit_text(&left, area.width as usize)
    };
    frame.render_widget(
        Paragraph::new(text).style(theme.style(VisualRole::Secondary)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.width >= 3 {
        frame.render_widget(
            Paragraph::new(" H ").style(theme.selected),
            Rect::new(area.x, area.y, 3, 1),
        );
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).style(theme.style(VisualRole::Muted)),
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
    }
}

fn draw_hero(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &HomeState,
    theme: &UiTheme,
    spacious: bool,
) {
    if area.height == 0 {
        return;
    }
    let wordmark_x = area.x + area.width.saturating_sub(WORDMARK_WIDTH) / 2;
    frame.render_widget(
        Paragraph::new(fit_text(
            "01 / HYDROGEN-1 / TERMINAL AGENT",
            WORDMARK_WIDTH as usize,
        ))
        .style(theme.style(VisualRole::Accent)),
        Rect::new(wordmark_x, area.y, WORDMARK_WIDTH.min(area.width), 1),
    );
    let wordmark_y = area.y.saturating_add(if spacious { 2 } else { 1 });
    draw_wordmark(
        frame,
        Rect::new(
            area.x,
            wordmark_y,
            area.width,
            area.bottom().saturating_sub(wordmark_y).min(5),
        ),
        theme,
    );
    let tagline_offset = if spacious { 8 } else { 7 };
    if area.height > tagline_offset {
        frame.render_widget(
            Paragraph::new(fit_text(
                "轻量到只剩行动。告诉我这次要在工作区完成什么。",
                area.width as usize,
            ))
            .alignment(Alignment::Center)
            .style(theme.style(VisualRole::Secondary)),
            Rect::new(area.x, area.y.saturating_add(tagline_offset), area.width, 1),
        );
    }
    let details_offset = if spacious { 10 } else { 8 };
    if area.height > details_offset {
        let details = join_ends(
            &format!("workspace / {}", state.workspace_label),
            "RUST/TOKIO  ·  BUILD  ·  LOCAL/WAL",
            area.width as usize,
        );
        frame.render_widget(
            Paragraph::new(details).style(theme.style(VisualRole::Muted)),
            Rect::new(area.x, area.y.saturating_add(details_offset), area.width, 1),
        );
    }
}

fn draw_wordmark(frame: &mut Frame<'_>, area: Rect, theme: &UiTheme) {
    const PROTIUM: [&str; 5] = [
        "  ██    ██  ██",
        " ███    ██  ██",
        "  ██    ██████",
        "  ██    ██  ██",
        "██████  ██  ██",
    ];
    const GLYPHS: [[&str; 5]; 6] = [
        ["      ", "      ", "██████", "      ", "      "],
        [" ████ ", "██  ██", "██████", "██  ██", "██  ██"],
        [" █████", "██    ", "██ ███", "██  ██", " ████ "],
        ["██████", "██    ", "█████ ", "██    ", "██████"],
        ["██  ██", "███ ██", "██████", "██ ███", "██  ██"],
        ["██████", "  ██  ", "  ██  ", "  ██  ", "  ██  "],
    ];

    for row in 0..usize::from(area.height.min(PROTIUM.len() as u16)) {
        let agent = [
            GLYPHS[0][row],
            GLYPHS[1][row],
            GLYPHS[2][row],
            GLYPHS[3][row],
            GLYPHS[4][row],
            GLYPHS[5][row],
        ]
        .join("  ");
        let wordmark_x = area.x + area.width.saturating_sub(WORDMARK_WIDTH) / 2;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(PROTIUM[row], theme.strong(VisualRole::Accent)),
                Span::raw("   "),
                Span::styled(agent, theme.strong(VisualRole::Primary)),
            ])),
            Rect::new(
                wordmark_x,
                area.y.saturating_add(row as u16),
                WORDMARK_WIDTH.min(area.width),
                1,
            ),
        );
    }
}

fn draw_compact_brand(frame: &mut Frame<'_>, area: Rect, state: &HomeState, theme: &UiTheme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new("1H-Agent / protium").style(theme.strong(VisualRole::Accent)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        let detail = join_ends(
            &format!("workspace / {}", state.workspace_label),
            "RUST/TOKIO · BUILD",
            area.width as usize,
        );
        frame.render_widget(
            Paragraph::new(detail).style(theme.style(VisualRole::Muted)),
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
    }
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, state: &mut HomeState, theme: &UiTheme) {
    state.input_rect = Some(area);
    let focused = state.focus == HomeFocus::Composer && !state.loading;
    let mode_label = state.mode.as_str().to_ascii_uppercase();
    let title = if state.loading {
        " 正在准备会话 ".to_owned()
    } else {
        format!(" > {mode_label} ")
    };
    if !state.loading {
        let prefix_width = UnicodeWidthStr::width(" > ") as u16;
        let mode_width = UnicodeWidthStr::width(mode_label.as_str()) as u16;
        let x = area.x.saturating_add(1).saturating_add(prefix_width);
        if mode_width > 0 && x.saturating_add(mode_width) <= area.right().saturating_sub(1) {
            state.mode_rect = Some(Rect::new(x, area.y, mode_width, 1));
        }
    }
    let block = Block::default()
        .title(Line::from(Span::styled(
            title,
            theme.strong(VisualRole::Accent),
        )))
        .borders(Borders::ALL)
        .border_style(if focused {
            theme.focus_border
        } else {
            theme.inactive_border
        });
    let inner = block.inner(area);
    let viewport = input_cursor_viewport(
        state.input.as_str(),
        state.input.cursor(),
        inner.width as usize,
    );
    frame.render_widget(
        Paragraph::new(if state.input.as_str().is_empty() && !state.loading {
            "输入一个任务，例如：检查当前实现并优化交互细节".to_owned()
        } else {
            viewport.text
        })
        .style(if state.input.as_str().is_empty() && !state.loading {
            theme.style(VisualRole::Muted)
        } else {
            theme.style(VisualRole::Primary)
        })
        .block(block),
        area,
    );
    if focused && inner.width > 0 && inner.height > 0 {
        let cursor_x = inner
            .x
            .saturating_add(viewport.cursor_column as u16)
            .min(inner.right().saturating_sub(1));
        let cursor_y = inner
            .y
            .saturating_add(viewport.cursor_row.min(inner.height.saturating_sub(1)));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_recents(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut HomeState,
    theme: &UiTheme,
    double_spaced: bool,
) {
    if area.height == 0 {
        return;
    }
    let row_height = if double_spaced { 2 } else { 1 };
    let slots = usize::from(area.height.saturating_sub(1) / row_height);
    let visible = slots
        .min(state.recent_sessions.len())
        .min(RECENT_SESSION_LIMIT);
    let header = join_ends(
        "RECENT SESSIONS",
        &format!("{visible:02} / {:02}", state.recent_sessions.len()),
        area.width as usize,
    );
    frame.render_widget(
        Paragraph::new(header).style(theme.strong(VisualRole::Secondary)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if visible == 0 {
        return;
    }

    for (index, session) in state.recent_sessions.iter().take(visible).enumerate() {
        let selected = state.focus == HomeFocus::Recents && index == state.selected_session;
        let row_y = area
            .y
            .saturating_add(1)
            .saturating_add((index as u16).saturating_mul(row_height));
        let row = Rect::new(area.x, row_y, area.width, row_height);
        state.session_rects.push((row, session.id.clone()));
        let marker_style = if selected {
            theme.strong(VisualRole::Accent)
        } else {
            theme.style(VisualRole::Muted)
        };
        let title_style = if selected {
            theme.strong(VisualRole::Primary)
        } else {
            theme.style(VisualRole::Primary)
        };
        let title_width = area.width.saturating_sub(8) as usize;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if selected { "▌" } else { " " }, marker_style),
                Span::raw(" "),
                Span::styled(format!("{:02}", index + 1), marker_style),
                Span::raw("  "),
                Span::styled(fit_text(&session.title, title_width), title_style),
            ])),
            Rect::new(area.x, row_y, area.width, 1),
        );
        if double_spaced && row_height > 1 {
            frame.render_widget(
                Paragraph::new("─".repeat(area.width as usize))
                    .style(theme.style(VisualRole::Muted)),
                Rect::new(area.x, row_y.saturating_add(1), area.width, 1),
            );
        }
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &mut HomeState, theme: &UiTheme) {
    if area.height == 0 {
        return;
    }
    let provider_label = state.provider_label();
    let model_label = state.provider.model.as_str();
    let metadata = format!("{provider_label} · {model_label}");
    let shortcuts = if state.loading {
        "正在初始化…"
    } else if state.recent_sessions.is_empty() {
        "Enter 开始  Ctrl+C 退出"
    } else {
        "Tab 最近会话  Enter 开始  Ctrl+C 退出"
    };
    let width = area.width as usize;
    let full_width = UnicodeWidthStr::width(metadata.as_str())
        .saturating_add(2)
        .saturating_add(UnicodeWidthStr::width(shortcuts));
    let text = if full_width <= width {
        format!(
            "{metadata}{}{shortcuts}",
            " ".repeat(width.saturating_sub(full_width))
        )
    } else if UnicodeWidthStr::width(shortcuts) <= width {
        shortcuts.to_owned()
    } else {
        fit_text(shortcuts, width)
    };
    if full_width <= width {
        let separator = " / ";
        let provider_width = UnicodeWidthStr::width(provider_label) as u16;
        let separator_width = UnicodeWidthStr::width(separator) as u16;
        let model_width = UnicodeWidthStr::width(model_label) as u16;
        state.provider_rect = (provider_width > 0)
            .then(|| Rect::new(area.x, area.y, provider_width.min(area.width), 1));
        let model_x = area
            .x
            .saturating_add(provider_width)
            .saturating_add(separator_width);
        state.model_rect = (model_width > 0 && model_x < area.right()).then(|| {
            Rect::new(
                model_x,
                area.y,
                model_width.min(area.right().saturating_sub(model_x)),
                1,
            )
        });
        let rest = format!("{separator}{model_label}");
        let spacing = " ".repeat(width.saturating_sub(full_width));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(provider_label, theme.style(VisualRole::Accent)),
                Span::styled(rest, theme.style(VisualRole::Secondary)),
                Span::raw(spacing),
                Span::styled(shortcuts, theme.style(VisualRole::Secondary)),
            ])),
            area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(text).style(theme.style(VisualRole::Secondary)),
            area,
        );
    }
}

fn draw_menu(
    frame: &mut Frame<'_>,
    screen: Rect,
    footer: Rect,
    state: &mut HomeState,
    theme: &UiTheme,
) {
    let Some(menu) = state.menu else {
        return;
    };
    let (labels, title, control, max_width) = match menu.kind {
        HomeMenuKind::Provider => (
            state
                .providers
                .iter()
                .map(|provider| provider.preset.label().to_owned())
                .collect::<Vec<_>>(),
            " 选择供应商 ".to_owned(),
            state.provider_rect,
            36,
        ),
        HomeMenuKind::Model => (
            state.model_choices(),
            format!(" {} 模型 ", state.provider_label()),
            state.model_rect,
            52,
        ),
    };
    if labels.is_empty() {
        state.menu = None;
        return;
    }
    let content_width = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(12)
        .saturating_add(4) as u16;
    let min_width = if menu.kind == HomeMenuKind::Provider {
        20
    } else {
        24
    };
    let width = content_width.clamp(min_width, max_width).min(screen.width);
    let height = (labels.len() as u16)
        .saturating_add(2)
        .min(14)
        .min(screen.height);
    let control = control.unwrap_or(Rect::new(footer.x, footer.y, 0, 0));
    let x = control.x.min(screen.right().saturating_sub(width));
    let y = footer.y.saturating_sub(height);
    let area = Rect::new(x, y, width, height);
    if let Some(active) = state.menu.as_mut() {
        active.rect = Some(area);
    }
    let visible = area.height.saturating_sub(2) as usize;
    let scroll = menu.selected.saturating_sub(visible.saturating_sub(1));
    let items = labels
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(index, label)| {
            let selected = index == menu.selected;
            ListItem::new(Line::from(Span::styled(
                format!("{} {label}", if selected { "›" } else { " " }),
                if selected {
                    theme.selected
                } else {
                    theme.style(VisualRole::Primary)
                },
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        area,
    );
}

fn join_ends(left: &str, right: &str, width: usize) -> String {
    let left_width = UnicodeWidthStr::width(left);
    let right_width = UnicodeWidthStr::width(right);
    if left_width.saturating_add(2).saturating_add(right_width) <= width {
        return format!(
            "{left}{}{right}",
            " ".repeat(width.saturating_sub(left_width).saturating_sub(right_width))
        );
    }
    fit_text(left, width)
}

fn fit_text(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let suffix = if width > 1 { "…" } else { "" };
    let budget = width.saturating_sub(UnicodeWidthStr::width(suffix));
    let mut output = String::new();
    let mut used = 0usize;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > budget {
            break;
        }
        output.push(character);
        used = used.saturating_add(character_width);
    }
    output.push_str(suffix);
    output
}

fn point_in_rect(column: u16, row: u16, rect: Rect) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderPreset;
    use crossterm::event::{KeyEvent, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    fn state() -> HomeState {
        let mut provider = ProviderPreset::OpenAi.defaults();
        provider.model = "gpt-5".into();
        HomeState::new(
            Path::new("/tmp/workspace"),
            provider.clone(),
            vec![provider, ProviderPreset::DeepSeek.defaults()],
            vec![
                SessionSummary {
                    id: "one".into(),
                    title: "第一个会话".into(),
                    parent_id: None,
                },
                SessionSummary {
                    id: "two".into(),
                    title: "second".into(),
                    parent_id: None,
                },
            ],
        )
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn composer_starts_only_with_non_empty_input() {
        let mut home = state();
        assert_eq!(
            home.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .action,
            None
        );
        home.handle_event(key(KeyCode::Char('你'), KeyModifiers::NONE));
        home.handle_event(key(KeyCode::Char('好'), KeyModifiers::NONE));
        assert_eq!(
            home.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .action,
            Some(HomeAction::StartNew("你好".into()))
        );
    }

    #[test]
    fn composer_supports_paste_multiline_and_quit() {
        let mut home = state();
        assert!(home.handle_event(Event::Paste("中文🙂".into())).redraw);
        home.handle_event(key(KeyCode::Enter, KeyModifiers::SHIFT));
        home.handle_event(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(home.input.as_str(), "中文🙂\nx");
        assert_eq!(
            home.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .action,
            Some(HomeAction::Quit)
        );
    }

    #[test]
    fn recent_sessions_use_explicit_focus_and_selection() {
        let mut home = state();
        home.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(home.focus, HomeFocus::Recents);
        home.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            home.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .action,
            Some(HomeAction::Resume("two".into()))
        );
        home.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(home.focus, HomeFocus::Composer);
    }

    #[test]
    fn mouse_click_on_recent_session_resumes_it() {
        let mut home = state();
        render(&mut home, 80, 20);
        let rect = home.session_rects[0].0;
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            home.handle_event(event).action,
            Some(HomeAction::Resume("one".into()))
        );
    }

    #[test]
    fn mode_provider_and_model_controls_apply_real_selections() {
        let mut home = state();
        render(&mut home, 80, 20);

        let mode = home.mode_rect.expect("mode control");
        assert!(home.handle_event(left_click(mode.x, mode.y)).redraw);
        assert_eq!(home.mode, AgentMode::Plan);
        let screen = render(&mut home, 80, 20);
        assert!(screen.iter().any(|line| line.contains("> PLAN")));

        let provider = home.provider_rect.expect("provider control");
        home.handle_event(left_click(provider.x, provider.y));
        render(&mut home, 80, 20);
        let menu = home.menu.expect("provider menu");
        let inner = Block::bordered().inner(menu.rect.expect("provider menu rect"));
        home.handle_event(left_click(inner.x, inner.y.saturating_add(1)));
        assert_eq!(home.provider.preset, ProviderPreset::DeepSeek);
        assert_eq!(home.provider.model, "deepseek-v4-flash");

        render(&mut home, 80, 20);
        let model = home.model_rect.expect("model control");
        home.handle_event(left_click(model.x, model.y));
        home.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
        home.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(home.provider.model, "deepseek-v4-pro");
        let selection = home.selection();
        assert_eq!(selection.mode, AgentMode::Plan);
        assert_eq!(selection.provider.preset, ProviderPreset::DeepSeek);
    }

    #[test]
    fn responsive_home_render_is_bounded() {
        for (width, height) in [(120, 30), (80, 20), (44, 14), (2, 2)] {
            let mut home = state();
            let screen = render(&mut home, width, height);
            assert_eq!(screen.len(), height as usize);
            assert!(
                screen
                    .iter()
                    .all(|line| line.chars().count() >= width as usize)
            );
            if width >= 44 && height >= 14 {
                assert!(screen.iter().any(|line| {
                    line.contains("1H-Agent") || line.contains("  ██    ██████")
                }));
                assert!(screen.iter().any(|line| line.contains("BUILD")));
                assert!(screen.iter().any(|line| line.contains("RECENT SESSIONS")));
                assert!(screen.iter().any(|line| line.contains("Tab")));
            }
        }

        let mut wide = state();
        let screen = render(&mut wide, 120, 30);
        assert!(screen.iter().any(|line| line.contains("HYDROGEN-1")));
        assert!(screen.iter().any(|line| line.contains("RUST/TOKIO")));
        assert!(screen.iter().any(|line| line.contains("  ██    ██████")));
        assert!(screen.iter().any(|line| line.contains("██████  ██  ██")));
    }

    #[test]
    fn wordmark_uses_blue_for_protium_and_white_for_agent() {
        let mut home = state();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut home)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..20)
            .map(|row| {
                let line = (0..80)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>();
                (row, line)
            })
            .find(|(_, line)| line.contains("  ██    ██████"))
            .map(|(row, _)| row)
            .expect("large 1H wordmark row");
        let last_blue = (0..80)
            .rfind(|column| {
                buffer[(*column, row)].symbol() == "█" && buffer[(*column, row)].fg == Color::Cyan
            })
            .expect("last blue wordmark cell");
        let first_white = (0..80)
            .find(|column| {
                buffer[(*column, row)].symbol() == "█" && buffer[(*column, row)].fg == Color::White
            })
            .expect("first white wordmark cell");
        assert_eq!(first_white.saturating_sub(last_blue), 4);
    }

    #[test]
    fn recent_focus_uses_the_theme_accent_marker() {
        let mut home = state();
        home.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut home)).unwrap();
        let marker = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "▌")
            .expect("selected recent session marker");
        assert_eq!(marker.fg, Color::Cyan);
    }

    fn render(home: &mut HomeState, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, home)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn left_click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }
}
