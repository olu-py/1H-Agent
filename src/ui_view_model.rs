use unicode_width::UnicodeWidthStr;

use crate::config::{ThinkingLevel, ThinkingProfileKind};
use crate::{
    app::{AgentPhase, App, DisplayContent, ToolDisplayStatus},
    commands::AgentMode,
    ui::tool_display_name,
    ui_layout::{Density, HeightClass},
    ui_theme::VisualRole,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityState {
    Idle,
    Working,
    Waiting,
    Success,
    Warning,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityView {
    pub state: ActivityState,
    pub symbol: &'static str,
    pub text: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSegment {
    pub text: String,
    pub role: VisualRole,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FooterLine {
    pub left: Vec<UiSegment>,
    pub right: Vec<UiSegment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FooterView {
    pub primary: FooterLine,
    pub secondary: Option<FooterLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputView {
    pub title: String,
    pub enabled: bool,
    pub warning: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextView {
    pub enabled: bool,
    pub used: u64,
    pub limit: Option<u64>,
    pub percent: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingMenuItem {
    pub level: ThinkingLevel,
    pub label: String,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingControlView {
    pub label: String,
    pub enabled: bool,
    pub options: Vec<ThinkingMenuItem>,
    pub qwen37_budgets: bool,
    pub budget_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutHint {
    pub key: &'static str,
    pub action: &'static str,
    pub priority: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiViewModel {
    pub activity: ActivityView,
    pub footer: FooterView,
    pub input: InputView,
    pub context: ContextView,
    pub thinking: ThinkingControlView,
    pub density: Density,
    pub height: HeightClass,
}

pub fn activity_view(app: &App) -> ActivityView {
    let (state, symbol, text) = if let Some(approval) = &app.current.pending_approval {
        (
            ActivityState::Warning,
            "!",
            format!("{}需要确认", tool_display_name(&approval.call.name)),
        )
    } else if app.current.agent_phase == AgentPhase::Failed {
        (ActivityState::Failed, "×", "请求失败".into())
    } else if app.current.agent_phase == AgentPhase::ToolRunning {
        let name = app
            .current
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                DisplayContent::Tool(tool) if tool.status == ToolDisplayStatus::Running => {
                    Some(tool_display_name(&tool.name))
                }
                _ => None,
            })
            .unwrap_or_else(|| "工具".into());
        (ActivityState::Working, "●", format!("正在执行：{name}"))
    } else if app.current.agent_phase == AgentPhase::Thinking {
        (ActivityState::Waiting, "●", "正在思考".into())
    } else if app.current.agent_phase == AgentPhase::StreamingText {
        (ActivityState::Working, "●", "正在生成回复".into())
    } else if app.current.agent_phase == AgentPhase::Completed {
        (ActivityState::Success, "✓", "已完成".into())
    } else if app.current.status.contains("已取消") {
        (ActivityState::Cancelled, "■", "已取消".into())
    } else {
        (ActivityState::Idle, "○", "就绪".into())
    };
    let detail = supplemental_status(&app.current.status, &text);
    ActivityView {
        state,
        symbol,
        text,
        detail,
    }
}

fn supplemental_status(status: &str, activity: &str) -> Option<String> {
    let cleaned = status
        .split('|')
        .next()
        .unwrap_or(status)
        .trim()
        .trim_end_matches(['…', '.']);
    if cleaned.is_empty()
        || cleaned == activity
        || cleaned.contains(activity)
        || activity.contains(cleaned)
        || matches!(cleaned, "就绪" | "请求失败" | "等待模型流式响应")
    {
        None
    } else {
        Some(cleaned.to_owned())
    }
}

pub fn contextual_shortcuts(app: &App) -> Vec<ShortcutHint> {
    if app.settings.is_some() {
        return vec![
            hint("Tab", "切换", 3),
            hint("Enter", "保存", 2),
            hint("Esc", "返回", 1),
        ];
    }
    if app.palette.is_some() {
        return vec![
            hint("↑↓", "选择", 3),
            hint("Enter", "执行", 2),
            hint("Esc", "关闭", 1),
        ];
    }
    if app.current.pending_approval.is_some() {
        return vec![hint("Y", "批准", 1), hint("N", "拒绝", 2)];
    }
    if app.current.busy {
        return vec![hint("Esc", "取消", 1)];
    }
    if !app.current.follow_output {
        return vec![hint("Ctrl+L", "回到底部", 1), hint("Enter", "发送", 3)];
    }
    if app.input.is_empty() {
        vec![hint("Enter", "发送", 1), hint("Ctrl+P", "命令", 2)]
    } else {
        vec![hint("Enter", "发送", 1), hint("Shift+Enter", "换行", 2)]
    }
}

const fn hint(key: &'static str, action: &'static str, priority: u8) -> ShortcutHint {
    ShortcutHint {
        key,
        action,
        priority,
    }
}

pub fn fit_shortcuts(hints: &[ShortcutHint], width: usize) -> Vec<ShortcutHint> {
    let mut output = hints.to_vec();
    while shortcuts_width(&output) > width {
        let Some((index, _)) = output
            .iter()
            .enumerate()
            .max_by_key(|(_, hint)| hint.priority)
        else {
            break;
        };
        output.remove(index);
    }
    output
}

fn shortcuts_width(hints: &[ShortcutHint]) -> usize {
    hints
        .iter()
        .map(|hint| UnicodeWidthStr::width(hint.key) + 1 + UnicodeWidthStr::width(hint.action))
        .sum::<usize>()
        .saturating_add(hints.len().saturating_sub(1) * 2)
}

impl UiViewModel {
    pub fn from_app(app: &App, density: Density, height: HeightClass, footer_width: usize) -> Self {
        let activity = activity_view(app);
        let context = ContextView {
            enabled: app.context_meter_enabled,
            used: app.current.context_used_tokens,
            limit: app.current.context_limit_tokens,
            percent: app.current.context_limit_tokens.map(|limit| {
                app.current
                    .context_used_tokens
                    .min(limit.max(1))
                    .saturating_mul(100)
                    / limit.max(1)
            }),
        };
        let profile = app.thinking_profile();
        let thinking = ThinkingControlView {
            label: format!("思考 {} ▾", app.thinking_level().label()),
            enabled: !app.current.busy
                && app.current.pending_approval.is_none()
                && app.settings.is_none()
                && app.palette.is_none(),
            options: profile
                .options
                .iter()
                .copied()
                .map(|level| ThinkingMenuItem {
                    level,
                    label: level.menu_label().into(),
                    selected: level == app.thinking_level(),
                })
                .collect(),
            qwen37_budgets: profile.kind == ThinkingProfileKind::Qwen37,
            budget_tokens: app.thinking_budget_tokens(),
        };
        let activity_width = UnicodeWidthStr::width(activity.text.as_str()) + 3;
        let shortcut_budget = footer_width.saturating_sub(activity_width.saturating_add(2));
        let shortcuts = fit_shortcuts(&contextual_shortcuts(app), shortcut_budget);
        let primary = FooterLine {
            left: vec![
                UiSegment {
                    text: format!("{} ", activity.symbol),
                    role: activity_role(activity.state),
                },
                UiSegment {
                    text: activity.text.clone(),
                    role: VisualRole::Primary,
                },
            ],
            right: shortcut_segments(&shortcuts),
        };
        let secondary = (height == HeightClass::Normal).then(|| {
            if let Some(approval) = &app.current.pending_approval {
                let path = approval
                    .call
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                FooterLine {
                    left: vec![UiSegment {
                        text: if path.is_empty() {
                            approval.reason.clone()
                        } else {
                            format!("{path} · {}", approval.reason)
                        },
                        role: VisualRole::Secondary,
                    }],
                    right: Vec::new(),
                }
            } else {
                FooterLine {
                    left: metadata_segments(
                        app.current.mode,
                        app.provider_label(),
                        app.model_name(),
                        density,
                    ),
                    right: thinking_segments(&thinking, &context, density),
                }
            }
        });
        Self {
            activity,
            footer: FooterView { primary, secondary },
            input: InputView {
                title: format!(" 输入 · {} ", mode_label(app.current.mode)),
                enabled: !app.current.busy && app.current.pending_approval.is_none(),
                warning: app.current.pending_approval.is_some(),
            },
            context,
            thinking,
            density,
            height,
        }
    }
}

fn thinking_segments(
    thinking: &ThinkingControlView,
    context: &ContextView,
    density: Density,
) -> Vec<UiSegment> {
    let mut output = Vec::new();
    let context = context_segments(context, density);
    if !context.is_empty() {
        output.extend(context);
        output.push(UiSegment {
            text: " · ".into(),
            role: VisualRole::Muted,
        });
    }
    output.push(UiSegment {
        text: thinking.label.clone(),
        role: if thinking.enabled {
            VisualRole::Accent
        } else {
            VisualRole::Muted
        },
    });
    output
}

fn activity_role(state: ActivityState) -> VisualRole {
    match state {
        ActivityState::Idle => VisualRole::Muted,
        ActivityState::Working | ActivityState::Waiting => VisualRole::Accent,
        ActivityState::Success => VisualRole::Success,
        ActivityState::Warning => VisualRole::Warning,
        ActivityState::Failed => VisualRole::Danger,
        ActivityState::Cancelled => VisualRole::Secondary,
    }
}

fn shortcut_segments(hints: &[ShortcutHint]) -> Vec<UiSegment> {
    let mut output = Vec::new();
    for (index, hint) in hints.iter().enumerate() {
        if index > 0 {
            output.push(UiSegment {
                text: "  ".into(),
                role: VisualRole::Muted,
            });
        }
        output.push(UiSegment {
            text: hint.key.into(),
            role: VisualRole::Shortcut,
        });
        output.push(UiSegment {
            text: format!(" {}", hint.action),
            role: VisualRole::Secondary,
        });
    }
    output
}

fn metadata_segments(
    mode: AgentMode,
    provider: &str,
    model: &str,
    density: Density,
) -> Vec<UiSegment> {
    let text = match density {
        Density::Wide => format!("{} · {provider} · {model}", mode_label(mode)),
        Density::Standard => format!("{} · {provider} · {model}", mode_label(mode)),
        Density::Compact => mode_label(mode).to_owned(),
    };
    vec![UiSegment {
        text,
        role: VisualRole::Secondary,
    }]
}

fn context_segments(context: &ContextView, density: Density) -> Vec<UiSegment> {
    if !context.enabled {
        return Vec::new();
    }
    let percent = context
        .percent
        .map_or("--".into(), |value| value.to_string());
    let text = match (density, context.limit) {
        (Density::Compact, _) => format!("上下文 {percent}%"),
        (_, Some(limit)) => format!(
            "上下文 {percent}% {}/{}",
            compact_tokens(context.used),
            compact_tokens(limit)
        ),
        (_, None) => format!("上下文 {percent}% {}", compact_tokens(context.used)),
    };
    vec![UiSegment {
        text,
        role: context.percent.map_or(VisualRole::Muted, |percent| {
            if percent >= 95 {
                VisualRole::Danger
            } else if percent >= 85 {
                VisualRole::Warning
            } else {
                VisualRole::Secondary
            }
        }),
    }]
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub fn mode_label(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => "构建",
        AgentMode::Plan => "计划",
        AgentMode::Explore => "探索",
        AgentMode::Cluster => "集群",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_fitting_drops_low_priority_items_first() {
        let hints = vec![hint("Enter", "发送", 1), hint("Ctrl+P", "命令", 3)];
        assert_eq!(fit_shortcuts(&hints, 12), vec![hints[0]]);
        assert!(fit_shortcuts(&hints, 2).is_empty());
    }

    #[test]
    fn supplemental_status_does_not_repeat_primary_activity() {
        assert_eq!(supplemental_status("请求失败", "请求失败"), None);
        assert_eq!(
            supplemental_status("正在输出正文…… | Esc 取消", "正在生成回复"),
            Some("正在输出正文".into())
        );
    }
}
