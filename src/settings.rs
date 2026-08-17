use std::{borrow::Cow, collections::HashSet};

use anyhow::Result;

use crate::{
    config::{ProviderConfig, ProviderKind, ProviderPreset, ThinkingCapability},
    secrets,
};

/// The provider popup is a small state machine: connected-profile list,
/// template picker, then the existing field editor.
pub enum SettingsState {
    List(ProviderList),
    Templates(TemplateList),
    Form(SettingsForm),
}

pub struct ProviderList {
    pub providers: Vec<ProviderConfig>,
    pub active: ProviderPreset,
    pub connected: HashSet<ProviderPreset>,
    /// Rows are profiles followed by the stable "add provider" command.
    pub selected: usize,
}

pub struct TemplateList {
    pub presets: Vec<ProviderPreset>,
    pub selected: usize,
}

impl SettingsState {
    pub fn list(providers: Vec<ProviderConfig>, active: ProviderPreset) -> Self {
        let connected = providers
            .iter()
            .filter_map(|provider| {
                secrets::api_key_cached_only(provider.preset)
                    .ok()
                    .map(|_| provider.preset)
            })
            .collect();
        Self::List(ProviderList {
            providers,
            active,
            connected,
            selected: 0,
        })
    }

    pub fn form(&self) -> Option<&SettingsForm> {
        match self {
            Self::Form(form) => Some(form),
            Self::List(_) | Self::Templates(_) => None,
        }
    }

    pub fn form_mut(&mut self) -> Option<&mut SettingsForm> {
        match self {
            Self::Form(form) => Some(form),
            Self::List(_) | Self::Templates(_) => None,
        }
    }

    pub fn move_selection(&mut self, direction: i32) {
        match self {
            Self::List(list) => {
                let len = list.providers.len().saturating_add(1) as i32;
                list.selected = (list.selected as i32 + direction).rem_euclid(len) as usize;
            }
            Self::Templates(templates) if !templates.presets.is_empty() => {
                let len = templates.presets.len() as i32;
                templates.selected =
                    (templates.selected as i32 + direction).rem_euclid(len) as usize;
            }
            Self::Form(form) => form.move_selection(direction),
            Self::Templates(_) => {}
        }
    }

    pub fn open_templates(&mut self) {
        let Self::List(list) = self else {
            return;
        };
        let presets = ProviderPreset::ALL
            .into_iter()
            .filter(|preset| {
                !list
                    .providers
                    .iter()
                    .any(|provider| provider.preset == *preset)
            })
            .collect();
        *self = Self::Templates(TemplateList {
            presets,
            selected: 0,
        });
    }

    pub fn selected_profile(&self) -> Option<ProviderConfig> {
        let Self::List(list) = self else {
            return None;
        };
        list.providers.get(list.selected).cloned()
    }

    pub fn on_add_row(&self) -> bool {
        matches!(self, Self::List(list) if list.selected == list.providers.len())
    }

    pub fn selected_template(&self) -> Option<ProviderPreset> {
        let Self::Templates(templates) = self else {
            return None;
        };
        templates.presets.get(templates.selected).copied()
    }
}

/// Stable identifier for each editable provider setting. Rendering, value
/// lookup, cycling, and text editing are all keyed off this enum, so adding a
/// field only touches this file plus the `FIELDS` registry below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsField {
    Preset,
    Protocol,
    Model,
    BaseUrl,
    Thinking,
    ApiKey,
}

/// Static description of a single settings row. All strings are `'static`, so
/// the registry costs nothing at runtime and can be iterated every frame.
#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    pub field: SettingsField,
    pub label: &'static str,
    pub section: &'static str,
}

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        field: SettingsField::Preset,
        label: "提供商",
        section: "基础",
    },
    FieldSpec {
        field: SettingsField::Protocol,
        label: "协议",
        section: "基础",
    },
    FieldSpec {
        field: SettingsField::Model,
        label: "模型",
        section: "基础",
    },
    FieldSpec {
        field: SettingsField::BaseUrl,
        label: "接口地址",
        section: "连接",
    },
    FieldSpec {
        field: SettingsField::Thinking,
        label: "思考能力",
        section: "高级",
    },
    FieldSpec {
        field: SettingsField::ApiKey,
        label: "API Key",
        section: "高级",
    },
];

/// In-memory edit buffer for the provider settings form. It owns exactly one
/// `ProviderConfig` copy plus one API key buffer; field values are exposed by
/// borrowing, so rendering does not clone large strings.
pub struct SettingsForm {
    pub provider: ProviderConfig,
    pub api_key: String,
    existing_key_preset: Option<ProviderPreset>,
    available_key_presets: HashSet<ProviderPreset>,
    pub selected: usize,
}

impl SettingsForm {
    pub fn new(provider: ProviderConfig, existing_key_preset: Option<ProviderPreset>) -> Self {
        Self {
            provider,
            api_key: String::new(),
            existing_key_preset,
            available_key_presets: HashSet::new(),
            selected: 0,
        }
    }

    pub fn set_available_key_presets(&mut self, presets: HashSet<ProviderPreset>) {
        self.available_key_presets = presets;
    }

    pub fn field(&self) -> SettingsField {
        FIELDS[self.selected].field
    }

    /// Whether a key already exists for the currently selected preset (in the
    /// running process, not the keyring). Drives the "********" placeholder.
    pub fn has_existing_key(&self) -> bool {
        self.existing_key_preset == Some(self.provider.preset)
            || self.available_key_presets.contains(&self.provider.preset)
    }

    pub fn move_selection(&mut self, direction: i32) {
        let len = FIELDS.len() as i32;
        self.selected = (self.selected as i32 + direction).rem_euclid(len) as usize;
    }

    /// Display value for a field. Borrows except for the API key mask, which is
    /// synthesized only while a key is being typed.
    pub fn value(&self, field: SettingsField) -> Cow<'_, str> {
        match field {
            SettingsField::Preset => Cow::Borrowed(self.provider.preset.label()),
            SettingsField::Protocol => Cow::Borrowed(self.provider.kind.label()),
            SettingsField::Model => Cow::Borrowed(&self.provider.model),
            SettingsField::BaseUrl => Cow::Borrowed(&self.provider.base_url),
            SettingsField::Thinking => Cow::Borrowed(self.provider.thinking.label()),
            SettingsField::ApiKey => {
                if !self.api_key.is_empty() {
                    Cow::Owned("*".repeat(self.api_key.chars().count().min(32)))
                } else if self.has_existing_key() {
                    Cow::Borrowed("********")
                } else {
                    Cow::Borrowed("（未设置）")
                }
            }
        }
    }

    /// Cycles an enumerated field. `Model` cycles the preset's selectable list
    /// and is a no-op for presets without candidates (Custom); text fields are
    /// handled by `edit` and are therefore untouched here.
    pub fn cycle(&mut self, field: SettingsField, direction: i32) {
        match field {
            // A profile's preset is its stable identity. Changing it would
            // bypass the one-profile-per-template rule, so it is read-only.
            SettingsField::Preset => {}
            SettingsField::Protocol => {
                if self.provider.preset.supports_responses() {
                    self.provider.kind = match self.provider.kind {
                        ProviderKind::ChatCompletions => ProviderKind::Responses,
                        ProviderKind::Responses => ProviderKind::ChatCompletions,
                    };
                }
            }
            SettingsField::Model => {
                let models = self.provider.preset.selectable_models();
                if models.is_empty() {
                    return;
                }
                let current = models
                    .iter()
                    .position(|model| *model == self.provider.model)
                    .unwrap_or(0) as i32;
                let next = (current + direction).rem_euclid(models.len() as i32) as usize;
                self.provider.model = models[next].to_owned();
            }
            SettingsField::Thinking => {
                let current = ThinkingCapability::ALL
                    .iter()
                    .position(|value| *value == self.provider.thinking)
                    .unwrap_or(0) as i32;
                let next =
                    (current + direction).rem_euclid(ThinkingCapability::ALL.len() as i32) as usize;
                self.provider.thinking = ThinkingCapability::ALL[next];
            }
            SettingsField::BaseUrl | SettingsField::ApiKey => {}
        }
    }

    /// Appends (`Some`) or deletes (`None`) a character for a text field.
    /// `Model` is editable too, so a custom ID can always be typed as a
    /// fallback alongside the picker list.
    pub fn edit(&mut self, field: SettingsField, character: Option<char>) {
        let value = match field {
            SettingsField::Model => &mut self.provider.model,
            SettingsField::BaseUrl => &mut self.provider.base_url,
            SettingsField::ApiKey => &mut self.api_key,
            _ => return,
        };
        match character {
            Some(character) => value.push(character),
            None => {
                value.pop();
            }
        }
    }

    /// Replaces the full value of a text field, used for paste operations
    /// where appending to the existing default (for example Base URL or model)
    /// would produce an invalid value.
    pub fn paste(&mut self, field: SettingsField, text: &str) {
        match field {
            SettingsField::Model => self.provider.model = text.to_owned(),
            SettingsField::BaseUrl => self.provider.base_url = text.to_owned(),
            SettingsField::ApiKey => self.api_key = text.to_owned(),
            _ => {}
        }
    }

    /// Returns a validated, normalized copy ready to commit. Unedited fields
    /// (native_web_search, context_window_tokens, …) pass through untouched
    /// because they live on the same `ProviderConfig` copy.
    pub fn prepare(&self) -> Result<ProviderConfig> {
        let mut provider = self.provider.clone();
        provider.validate()?;
        provider.normalize_thinking();
        Ok(provider)
    }

    pub fn resolve_api_key(&self, active: Option<&(ProviderPreset, String)>) -> Result<String> {
        let entered = self.api_key.trim();
        if !entered.is_empty() {
            return Ok(entered.to_owned());
        }
        match active {
            Some((preset, key)) if *preset == self.provider.preset => Ok(key.clone()),
            _ => Ok(secrets::api_key_cached_only(self.provider.preset)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_unique_and_grouped() {
        let mut seen = std::collections::HashSet::new();
        for spec in FIELDS {
            assert!(!spec.label.is_empty());
            assert!(!spec.section.is_empty());
            assert!(seen.insert(spec.field));
        }
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn preset_is_read_only_inside_a_connection_profile() {
        let mut form = SettingsForm::new(ProviderPreset::OpenAi.defaults(), None);
        form.api_key = "typed".into();
        form.cycle(SettingsField::Preset, 1);
        assert_eq!(form.provider.preset, ProviderPreset::OpenAi);
        assert_eq!(form.provider.model, "gpt-5-mini");
        assert_eq!(form.api_key, "typed");
    }

    #[test]
    fn template_picker_excludes_connected_presets() {
        let mut state = SettingsState::list(
            vec![
                ProviderPreset::OpenAi.defaults(),
                ProviderPreset::DeepSeek.defaults(),
            ],
            ProviderPreset::OpenAi,
        );
        state.open_templates();
        let SettingsState::Templates(templates) = state else {
            panic!("expected template picker");
        };
        assert!(!templates.presets.contains(&ProviderPreset::OpenAi));
        assert!(!templates.presets.contains(&ProviderPreset::DeepSeek));
        assert!(templates.presets.contains(&ProviderPreset::Qwen));
    }

    #[test]
    fn model_cycle_follows_selectable_list_and_skips_custom() {
        let mut form = SettingsForm::new(ProviderPreset::OpenAi.defaults(), None);
        form.cycle(SettingsField::Model, 1);
        assert_eq!(form.provider.model, "gpt-5");

        let mut custom = SettingsForm::new(ProviderPreset::Custom.defaults(), None);
        let before = custom.provider.model.clone();
        custom.cycle(SettingsField::Model, 1);
        assert_eq!(custom.provider.model, before);
    }

    #[test]
    fn model_edit_is_the_manual_fallback() {
        let mut form = SettingsForm::new(ProviderPreset::Custom.defaults(), None);
        form.edit(SettingsField::Model, Some('x'));
        form.edit(SettingsField::Model, Some('1'));
        assert_eq!(form.provider.model, "model-namex1");
        form.edit(SettingsField::Model, None);
        assert_eq!(form.provider.model, "model-namex");
    }

    #[test]
    fn value_masks_api_key_and_borrows_others() {
        let mut form = SettingsForm::new(ProviderPreset::OpenAi.defaults(), None);
        assert_eq!(form.value(SettingsField::ApiKey), "（未设置）");
        form.api_key = "sk-secret".into();
        assert_eq!(form.value(SettingsField::ApiKey), "*********");
        assert_eq!(form.value(SettingsField::Model), "gpt-5-mini");
    }

    #[test]
    fn has_existing_key_respects_available_provider_set() {
        let mut form = SettingsForm::new(ProviderPreset::DeepSeek.defaults(), None);
        assert!(!form.has_existing_key());

        form.set_available_key_presets(HashSet::from([ProviderPreset::DeepSeek]));
        assert!(form.has_existing_key());
        assert_eq!(form.value(SettingsField::ApiKey), "********");

        form.provider = ProviderPreset::Qwen.defaults();
        assert!(!form.has_existing_key());
    }

    #[test]
    fn prepare_validates_and_normalizes() {
        let mut form = SettingsForm::new(ProviderPreset::OpenAi.defaults(), None);
        form.provider.base_url = " https://api.openai.com/v1/ ".into();
        let prepared = form.prepare().unwrap();
        assert_eq!(prepared.base_url, "https://api.openai.com/v1");
    }
}
