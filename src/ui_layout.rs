use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders},
};

pub const WIDE_MIN_WIDTH: u16 = 100;
pub const STANDARD_MIN_WIDTH: u16 = 70;
pub const SHORT_MAX_HEIGHT: u16 = 15;
pub const SIDEBAR_WIDTH: u16 = 30;
pub const INPUT_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Compact,
    Standard,
    Wide,
}

impl Density {
    pub fn from_width(width: u16) -> Self {
        if width >= WIDE_MIN_WIDTH {
            Self::Wide
        } else if width >= STANDARD_MIN_WIDTH {
            Self::Standard
        } else {
            Self::Compact
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeightClass {
    Short,
    Normal,
}

impl HeightClass {
    pub fn from_height(height: u16) -> Self {
        if height <= SHORT_MAX_HEIGHT {
            Self::Short
        } else {
            Self::Normal
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiLayout {
    pub sessions: Option<Rect>,
    pub messages_outer: Rect,
    pub messages_inner: Rect,
    pub input: Rect,
    pub footer: Rect,
}

pub fn message_block() -> Block<'static> {
    Block::default().title(" 任务 ").borders(Borders::BOTTOM)
}

pub fn compute_layout(area: Rect, density: Density, height: HeightClass) -> UiLayout {
    let footer_height = if height == HeightClass::Short { 1 } else { 2 };
    let input_height = INPUT_HEIGHT.min(area.height.saturating_sub(footer_height));
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(input_height),
            Constraint::Length(footer_height.min(area.height)),
        ])
        .split(area);
    let content = vertical[0];
    let (sessions, messages_outer) = if density == Density::Wide {
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(SIDEBAR_WIDTH.min(content.width)),
                Constraint::Min(0),
            ])
            .split(content);
        (Some(horizontal[0]), horizontal[1])
    } else {
        (None, content)
    };
    let messages_inner = message_block().inner(messages_outer);
    UiLayout {
        sessions,
        messages_outer,
        messages_inner,
        input: vertical[1],
        footer: vertical[2],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    #[test]
    fn responsive_rects_are_bounded_and_use_block_inner() {
        for area in [
            Rect::new(0, 0, 120, 30),
            Rect::new(0, 0, 80, 20),
            Rect::new(0, 0, 44, 14),
            Rect::new(0, 0, 2, 2),
            Rect::new(0, 0, 0, 0),
        ] {
            let density = Density::from_width(area.width);
            let height = HeightClass::from_height(area.height);
            let layout = compute_layout(area, density, height);
            assert!(inside(layout.messages_outer, area));
            assert!(inside(layout.messages_inner, layout.messages_outer));
            assert!(inside(layout.input, area));
            assert!(inside(layout.footer, area));
            assert_eq!(
                layout.messages_inner,
                message_block().inner(layout.messages_outer)
            );
            assert!(layout.messages_outer.bottom() <= layout.input.y);
            assert!(layout.input.bottom() <= layout.footer.y);
            assert_eq!(layout.sessions.is_some(), density == Density::Wide);
            assert_eq!(
                layout.footer.height,
                if height == HeightClass::Short { 1 } else { 2 }.min(area.height)
            );
        }
    }

    #[test]
    fn density_thresholds_are_centralized() {
        assert_eq!(Density::from_width(69), Density::Compact);
        assert_eq!(Density::from_width(70), Density::Standard);
        assert_eq!(Density::from_width(99), Density::Standard);
        assert_eq!(Density::from_width(100), Density::Wide);
    }
}
