use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualRole {
    Primary,
    Secondary,
    Muted,
    Accent,
    User,
    Thinking,
    Tool,
    Success,
    Warning,
    Danger,
    Shortcut,
    Code,
    CodeMeta,
}

#[derive(Clone, Copy, Debug)]
pub struct UiTheme {
    pub focus_border: Style,
    pub inactive_border: Style,
    pub selected: Style,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            focus_border: Style::default().fg(Color::Cyan),
            inactive_border: Style::default().fg(Color::DarkGray),
            selected: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        }
    }
}

impl UiTheme {
    pub fn style(self, role: VisualRole) -> Style {
        let color = match role {
            VisualRole::Primary => Color::White,
            VisualRole::Secondary => Color::Gray,
            VisualRole::Muted => Color::DarkGray,
            VisualRole::Accent => Color::Cyan,
            VisualRole::User | VisualRole::Success => Color::Green,
            VisualRole::Thinking => Color::Magenta,
            VisualRole::Tool | VisualRole::Warning => Color::Yellow,
            VisualRole::Danger => Color::Red,
            VisualRole::Shortcut => Color::Cyan,
            VisualRole::Code => Color::Gray,
            VisualRole::CodeMeta => Color::DarkGray,
        };
        Style::default().fg(color)
    }

    pub fn strong(self, role: VisualRole) -> Style {
        self.style(role).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn semantic_roles_have_consistent_colors() {
        let theme = UiTheme::default();
        assert_eq!(theme.style(VisualRole::Primary).fg, Some(Color::White));
        assert_eq!(theme.style(VisualRole::Accent).fg, Some(Color::Cyan));
        assert_eq!(theme.style(VisualRole::Thinking).fg, Some(Color::Magenta));
        assert_eq!(theme.style(VisualRole::Tool).fg, Some(Color::Yellow));
        assert_eq!(theme.style(VisualRole::Danger).fg, Some(Color::Red));
    }
}
