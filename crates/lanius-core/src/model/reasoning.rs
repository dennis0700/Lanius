//! Native thinking/reasoning support, derived from Kiro's model catalog.
//!
//! Kiro's `ListAvailableModels` response describes, per model, which extra
//! request fields the model accepts (`additionalModelRequestFieldsSchema`).
//! Two families expose native reasoning there:
//!
//! - **Claude-style** ([`ReasoningProtocol::Thinking`]): a `thinking` object
//!   (`type`: e.g. `adaptive`/`disabled`, `display`: `summarized`/`omitted`)
//!   plus an optional `output_config.effort` level.
//! - **GPT-style** ([`ReasoningProtocol::Reasoning`]): a `reasoning.effort`
//!   level.
//!
//! Models without either field get no reasoning fields at all, and no
//! thinking output. [`ReasoningCapability::from_model`] parses the schema,
//! and [`ReasoningCapability::request_fields`] turns a client's
//! [`ReasoningRequest`] into the `additionalModelRequestFields` value sent
//! upstream, snapping requested effort levels to the nearest level the
//! model actually supports.

use serde_json::{Map, Value, json};

/// A reasoning-effort level, ordered from least to most effort. Covers the
/// union of the levels used by OpenAI clients (`none`..`xhigh`) and Kiro's
/// model schemas (`low`..`max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffortLevel {
    /// No reasoning at all.
    None,
    /// Minimal reasoning effort (OpenAI-style).
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium (default) reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort (OpenAI-style).
    Xhigh,
    /// Maximum reasoning effort (Kiro-style).
    Max,
}

impl EffortLevel {
    /// The wire string for this level.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::EffortLevel;
    ///
    /// assert_eq!(EffortLevel::Xhigh.as_str(), "xhigh");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses a wire string (case-insensitive), returning `None` for
    /// unknown levels.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::EffortLevel;
    ///
    /// assert_eq!(EffortLevel::parse("HIGH"), Some(EffortLevel::High));
    /// assert_eq!(EffortLevel::parse("turbo"), None);
    /// ```
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn rank(self) -> i32 {
        self as i32
    }
}

/// Models that stream native thinking (`reasoningContentEvent`) even though
/// their catalog entry declares no reasoning schema. Verified against Kiro
/// (2026-09): both MiniMax models return their thinking text unprompted.
pub const THINKING_WITHOUT_SCHEMA: &[&str] = &["minimax-m2.1", "minimax-m2.5"];

/// Whether `model_id` returns visible thinking text: Claude-style schema
/// models (GPT-style models reason, but Kiro hides the text) plus the
/// [`THINKING_WITHOUT_SCHEMA`] models.
///
/// # Examples
///
/// ```
/// use lanius_core::model::returns_visible_thinking;
///
/// assert!(returns_visible_thinking("minimax-m2.5", None));
/// assert!(!returns_visible_thinking("claude-sonnet-4.5", None));
/// ```
pub fn returns_visible_thinking(model_id: &str, capability: Option<&ReasoningCapability>) -> bool {
    capability.is_some_and(|capability| capability.protocol == ReasoningProtocol::Thinking)
        || THINKING_WITHOUT_SCHEMA.contains(&model_id)
}

/// Which request field family a model uses for native reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningProtocol {
    /// Claude-style `thinking` object plus optional `output_config.effort`.
    Thinking,
    /// GPT-style `reasoning.effort`.
    Reasoning,
}

/// What a client asked for, normalized across the OpenAI and Anthropic
/// request shapes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReasoningRequest {
    /// The client explicitly asked for reasoning to be off.
    pub disabled: bool,
    /// Requested effort level, if any.
    pub effort: Option<EffortLevel>,
    /// Requested Claude thinking display mode (`summarized`/`omitted`), if any.
    pub display: Option<String>,
}

/// A model's native reasoning support, as declared by its catalog schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningCapability {
    /// Which client-facing reasoning protocol the model follows (Claude-style
    /// `thinking` block vs. GPT-style `reasoning_effort`).
    pub protocol: ReasoningProtocol,
    /// Allowed `thinking.type` values (Claude-style only).
    pub thinking_types: Vec<String>,
    /// Allowed `thinking.display` values (Claude-style only).
    pub display_modes: Vec<String>,
    /// Effort levels the model accepts, in schema order.
    pub effort_levels: Vec<EffortLevel>,
    /// The schema's default effort level, if declared.
    pub default_effort: Option<EffortLevel>,
}

impl ReasoningCapability {
    /// Parses a catalog entry (one element of `ListAvailableModels`'s
    /// `models` array). Returns `None` when the model declares neither a
    /// `thinking` nor a `reasoning` field.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ReasoningCapability, ReasoningProtocol};
    /// use serde_json::json;
    ///
    /// let model = json!({"additionalModelRequestFieldsSchema": {"properties": {
    ///     "reasoning": {"properties": {"effort": {"enum": ["none", "low", "high"], "default": "high"}}}
    /// }}});
    /// let capability = ReasoningCapability::from_model(&model).unwrap();
    /// assert_eq!(capability.protocol, ReasoningProtocol::Reasoning);
    /// assert!(ReasoningCapability::from_model(&json!({"modelId": "old"})).is_none());
    /// ```
    pub fn from_model(model: &Value) -> Option<Self> {
        let properties = model
            .get("additionalModelRequestFieldsSchema")?
            .get("properties")?;
        if let Some(thinking) = properties.get("thinking") {
            let effort = properties.pointer("/output_config/properties/effort");
            return Some(Self {
                protocol: ReasoningProtocol::Thinking,
                thinking_types: string_enum(thinking.pointer("/properties/type")),
                display_modes: string_enum(thinking.pointer("/properties/display")),
                effort_levels: effort_enum(effort),
                default_effort: effort_default(effort),
            });
        }
        let effort = properties.pointer("/reasoning/properties/effort")?;
        Some(Self {
            protocol: ReasoningProtocol::Reasoning,
            thinking_types: Vec::new(),
            display_modes: Vec::new(),
            effort_levels: effort_enum(Some(effort)),
            default_effort: effort_default(Some(effort)),
        })
    }

    /// Whether reasoning can be switched off entirely for this model.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{EffortLevel, ReasoningCapability, ReasoningProtocol};
    ///
    /// let capability = ReasoningCapability {
    ///     protocol: ReasoningProtocol::Reasoning,
    ///     thinking_types: vec![],
    ///     display_modes: vec![],
    ///     effort_levels: vec![EffortLevel::None, EffortLevel::Low, EffortLevel::High],
    ///     default_effort: None,
    /// };
    /// assert!(capability.can_disable());
    /// ```
    pub fn can_disable(&self) -> bool {
        match self.protocol {
            ReasoningProtocol::Thinking => self.thinking_types.iter().any(|t| t == "disabled"),
            ReasoningProtocol::Reasoning => self.effort_levels.contains(&EffortLevel::None),
        }
    }

    /// Snaps `requested` to the closest supported effort level (ties go to
    /// the lower level). `none` is only ever chosen when `none` itself was
    /// requested, so asking for a small effort never silently switches
    /// reasoning off. Returns `None` if the model declares no usable levels.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{EffortLevel, ReasoningCapability, ReasoningProtocol};
    ///
    /// let capability = ReasoningCapability {
    ///     protocol: ReasoningProtocol::Thinking,
    ///     thinking_types: vec!["adaptive".into()],
    ///     display_modes: vec![],
    ///     effort_levels: vec![EffortLevel::Low, EffortLevel::High, EffortLevel::Max],
    ///     default_effort: None,
    /// };
    /// assert_eq!(capability.nearest_effort(EffortLevel::Minimal), Some(EffortLevel::Low));
    /// assert_eq!(capability.nearest_effort(EffortLevel::Medium), Some(EffortLevel::Low));
    /// assert_eq!(capability.nearest_effort(EffortLevel::Xhigh), Some(EffortLevel::High));
    /// ```
    pub fn nearest_effort(&self, requested: EffortLevel) -> Option<EffortLevel> {
        self.effort_levels
            .iter()
            .copied()
            .filter(|level| *level != EffortLevel::None || requested == EffortLevel::None)
            .min_by_key(|level| ((level.rank() - requested.rank()).abs(), level.rank()))
    }

    /// Builds the `additionalModelRequestFields` value for a request, or
    /// `None` when nothing needs to be sent.
    ///
    /// Claude-style models always get a `thinking` object so the summarized
    /// thinking text is streamed back (unless the client disabled thinking
    /// and the model allows it); an effort level is only added when the
    /// client asked for one. GPT-style models only get fields when the client
    /// requested an effort or disabled reasoning, otherwise the model default
    /// applies. A disable request on a model that cannot disable reasoning
    /// falls back to its lowest effort level.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ReasoningCapability, ReasoningRequest};
    /// use serde_json::json;
    ///
    /// let model = json!({"additionalModelRequestFieldsSchema": {"properties": {
    ///     "thinking": {"properties": {
    ///         "type": {"enum": ["adaptive"]},
    ///         "display": {"enum": ["summarized", "omitted"]}
    ///     }}
    /// }}});
    /// let capability = ReasoningCapability::from_model(&model).unwrap();
    /// assert_eq!(
    ///     capability.request_fields(&ReasoningRequest::default()),
    ///     Some(json!({"thinking": {"type": "adaptive", "display": "summarized"}}))
    /// );
    /// ```
    pub fn request_fields(&self, request: &ReasoningRequest) -> Option<Value> {
        let requested_effort = if request.disabled {
            Some(EffortLevel::None)
        } else {
            request.effort
        };
        let effort = requested_effort.and_then(|level| self.nearest_effort(level));
        let mut fields = Map::new();
        match self.protocol {
            ReasoningProtocol::Thinking => {
                if request.disabled && self.can_disable() {
                    fields.insert("thinking".into(), json!({"type": "disabled"}));
                    return Some(Value::Object(fields));
                }
                let kind = self
                    .thinking_types
                    .iter()
                    .find(|t| t.as_str() == "adaptive")
                    .or_else(|| {
                        self.thinking_types
                            .iter()
                            .find(|t| t.as_str() != "disabled")
                    })?;
                let mut thinking = Map::new();
                thinking.insert("type".into(), Value::String(kind.clone()));
                let display = request
                    .display
                    .as_ref()
                    .filter(|mode| self.display_modes.contains(mode))
                    .or_else(|| {
                        self.display_modes
                            .iter()
                            .find(|m| m.as_str() == "summarized")
                    });
                if let Some(display) = display {
                    thinking.insert("display".into(), Value::String(display.clone()));
                }
                fields.insert("thinking".into(), Value::Object(thinking));
                if let Some(effort) = effort {
                    fields.insert("output_config".into(), json!({"effort": effort.as_str()}));
                }
            }
            ReasoningProtocol::Reasoning => {
                let effort = effort?;
                fields.insert("reasoning".into(), json!({"effort": effort.as_str()}));
            }
        }
        Some(Value::Object(fields))
    }
}

fn string_enum(schema: Option<&Value>) -> Vec<String> {
    schema
        .and_then(|schema| schema.get("enum"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn effort_enum(schema: Option<&Value>) -> Vec<EffortLevel> {
    string_enum(schema)
        .iter()
        .filter_map(|level| EffortLevel::parse(level))
        .collect()
}

fn effort_default(schema: Option<&Value>) -> Option<EffortLevel> {
    schema
        .and_then(|schema| schema.get("default"))
        .and_then(Value::as_str)
        .and_then(EffortLevel::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(types: &[&str]) -> Value {
        json!({"modelId": "claude", "additionalModelRequestFieldsSchema": {"properties": {
            "thinking": {"properties": {
                "type": {"enum": types},
                "display": {"enum": ["summarized", "omitted"]}
            }},
            "output_config": {"properties": {"effort": {
                "enum": ["low", "medium", "high", "xhigh", "max"], "default": "high"
            }}},
            "max_tokens": {"type": "integer"}
        }}})
    }

    fn gpt() -> Value {
        json!({"modelId": "gpt", "additionalModelRequestFieldsSchema": {"properties": {
            "reasoning": {"properties": {"effort": {
                "enum": ["none", "low", "medium", "high", "xhigh", "max"], "default": "high"
            }}}
        }}})
    }

    #[test]
    fn parses_claude_gpt_and_missing_schemas() {
        let capability = ReasoningCapability::from_model(&claude(&["adaptive", "disabled"]))
            .expect("claude schema");
        assert_eq!(capability.protocol, ReasoningProtocol::Thinking);
        assert_eq!(capability.effort_levels.len(), 5);
        assert_eq!(capability.default_effort, Some(EffortLevel::High));
        assert!(capability.can_disable());

        let capability = ReasoningCapability::from_model(&gpt()).expect("gpt schema");
        assert_eq!(capability.protocol, ReasoningProtocol::Reasoning);
        assert!(capability.can_disable());

        assert!(
            ReasoningCapability::from_model(&json!({"modelId": "claude-sonnet-4.5"})).is_none()
        );
        assert!(
            ReasoningCapability::from_model(&json!({
                "additionalModelRequestFieldsSchema": {"properties": {"max_tokens": {}}}
            }))
            .is_none()
        );
    }

    #[test]
    fn claude_defaults_to_adaptive_summarized_without_effort() {
        let capability =
            ReasoningCapability::from_model(&claude(&["adaptive", "disabled"])).unwrap();
        assert_eq!(
            capability.request_fields(&ReasoningRequest::default()),
            Some(json!({"thinking": {"type": "adaptive", "display": "summarized"}}))
        );
    }

    #[test]
    fn claude_effort_snaps_to_supported_levels() {
        let capability = ReasoningCapability::from_model(&claude(&["adaptive"])).unwrap();
        let fields = |effort| {
            capability
                .request_fields(&ReasoningRequest {
                    effort: Some(effort),
                    ..Default::default()
                })
                .unwrap()["output_config"]["effort"]
                .clone()
        };
        assert_eq!(fields(EffortLevel::Minimal), "low");
        assert_eq!(fields(EffortLevel::Medium), "medium");
        assert_eq!(fields(EffortLevel::Xhigh), "xhigh");
        assert_eq!(fields(EffortLevel::Max), "max");
    }

    #[test]
    fn claude_disable_respects_whether_the_model_allows_it() {
        let disable = ReasoningRequest {
            disabled: true,
            ..Default::default()
        };
        let toggleable =
            ReasoningCapability::from_model(&claude(&["adaptive", "disabled"])).unwrap();
        assert_eq!(
            toggleable.request_fields(&disable),
            Some(json!({"thinking": {"type": "disabled"}}))
        );

        let always_on = ReasoningCapability::from_model(&claude(&["adaptive"])).unwrap();
        assert_eq!(
            always_on.request_fields(&disable),
            Some(json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "low"}
            }))
        );
    }

    #[test]
    fn claude_display_is_honored_only_when_supported() {
        let capability = ReasoningCapability::from_model(&claude(&["adaptive"])).unwrap();
        let with_display = |display: &str| {
            capability
                .request_fields(&ReasoningRequest {
                    display: Some(display.into()),
                    ..Default::default()
                })
                .unwrap()["thinking"]["display"]
                .clone()
        };
        assert_eq!(with_display("omitted"), "omitted");
        assert_eq!(with_display("verbose"), "summarized");
    }

    #[test]
    fn gpt_sends_fields_only_when_requested() {
        let capability = ReasoningCapability::from_model(&gpt()).unwrap();
        assert_eq!(
            capability.request_fields(&ReasoningRequest::default()),
            None
        );
        assert_eq!(
            capability.request_fields(&ReasoningRequest {
                effort: Some(EffortLevel::Minimal),
                ..Default::default()
            }),
            Some(json!({"reasoning": {"effort": "low"}}))
        );
        assert_eq!(
            capability.request_fields(&ReasoningRequest {
                disabled: true,
                ..Default::default()
            }),
            Some(json!({"reasoning": {"effort": "none"}}))
        );
    }
}
