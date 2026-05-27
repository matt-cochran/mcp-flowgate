//! YAML loader for `agents.yaml`. All types here are produced from a
//! strict deserialisation (`#[serde(deny_unknown_fields)]` per struct,
//! mandatory `default:` field with no `#[serde(default)]`).
//!
//! Per-provider feature structs (`AnthropicFeatures`, `OpenAIFeatures`,
//! `GoogleFeatures`) also use `deny_unknown_fields` so a typo like
//! `reasoning_effrt: high` fails at load with the offending key named —
//! FMECA T3 mitigation.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::Deserialize;

// ── closed enums (locked design) ────────────────────────────────────────────

/// What the model is being asked to do. Closed by design — the resolver
/// matches on this for sparse overrides. Enum additions are minor-version
/// compatible; removals are major. See `/guides/agent-config.mdx` for the
/// versioning policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Affinity {
    Coding,
    Reasoning,
    Prose,
    WebSearch,
    Recon,
}

impl fmt::Display for Affinity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Affinity::Coding => "coding",
            Affinity::Reasoning => "reasoning",
            Affinity::Prose => "prose",
            Affinity::WebSearch => "web-search",
            Affinity::Recon => "recon",
        };
        f.write_str(s)
    }
}

impl FromStr for Affinity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "coding" => Affinity::Coding,
            "reasoning" => Affinity::Reasoning,
            "prose" => Affinity::Prose,
            "web-search" => Affinity::WebSearch,
            "recon" => Affinity::Recon,
            other => return Err(other.to_string()),
        })
    }
}

/// Capability tier. Same versioning policy as `Affinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Frontier,
    Standard,
    Commoditized,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tier::Frontier => "frontier",
            Tier::Standard => "standard",
            Tier::Commoditized => "commoditized",
        };
        f.write_str(s)
    }
}

impl FromStr for Tier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "frontier" => Tier::Frontier,
            "standard" => Tier::Standard,
            "commoditized" => Tier::Commoditized,
            other => return Err(other.to_string()),
        })
    }
}

/// Provider enum with one `Custom { endpoint }` escape hatch for
/// self-hosted or non-listed providers that expose an OpenAI-shaped API.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "name", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Provider {
    Anthropic,
    Openai,
    Google,
    Ollama,
    Lmstudio,
    /// Self-hosted / unlisted provider. `endpoint` is required at load
    /// time — empty/missing → `AgentConfigError::ProviderEndpointRequired`.
    Custom { endpoint: String },
}

impl Provider {
    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
            Provider::Google => "google",
            Provider::Ollama => "ollama",
            Provider::Lmstudio => "lmstudio",
            Provider::Custom { .. } => "custom",
        }
    }
}

// ── feature toggle structs (closed; `deny_unknown_fields`) ──────────────────

/// Anthropic-specific feature toggles. Typos like `extendd_thinking` fail
/// at load with the field named (FMECA T3 mitigation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AnthropicFeatures {
    #[serde(default)]
    pub extended_thinking: bool,
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct OpenAIFeatures {
    /// `low` | `medium` | `high`. String, not enum, because OpenAI's API
    /// accepts a few additional values we don't want to fix in code.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GoogleFeatures {
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
}

/// Per-provider feature set on a `Binding`. Discriminated by provider so
/// a binding with `provider: anthropic` accepts only Anthropic feature
/// keys; OpenAI flags on an Anthropic binding fail at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProviderFeatures {
    Anthropic(AnthropicFeatures),
    OpenAI(OpenAIFeatures),
    Google(GoogleFeatures),
    /// Providers without typed feature toggles (Ollama, LmStudio, Custom).
    #[default]
    None,
}

// ── binding ─────────────────────────────────────────────────────────────────

/// One concrete binding: the provider + model the resolver will spawn a
/// sub-agent against, plus the typed feature toggles for that provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub provider: Provider,
    pub model: String,
    pub features: ProviderFeatures,
}

/// On-disk shape (before features are typed per-provider).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    provider: Provider,
    model: String,
    #[serde(default)]
    features: Option<serde_yaml::Value>,
}

impl RawBinding {
    fn into_binding(self) -> Result<Binding, AgentConfigError> {
        if self.model.trim().is_empty() {
            return Err(AgentConfigError::MissingProviderModel);
        }
        if let Provider::Custom { endpoint } = &self.provider {
            if endpoint.trim().is_empty() {
                return Err(AgentConfigError::ProviderEndpointRequired);
            }
        }
        let features = match (&self.provider, self.features) {
            (Provider::Anthropic, Some(v)) => ProviderFeatures::Anthropic(
                serde_yaml::from_value::<AnthropicFeatures>(v)
                    .map_err(|e| feature_error("anthropic", e))?,
            ),
            (Provider::Openai, Some(v)) => ProviderFeatures::OpenAI(
                serde_yaml::from_value::<OpenAIFeatures>(v)
                    .map_err(|e| feature_error("openai", e))?,
            ),
            (Provider::Google, Some(v)) => ProviderFeatures::Google(
                serde_yaml::from_value::<GoogleFeatures>(v).map_err(|e| feature_error("google", e))?,
            ),
            (Provider::Ollama | Provider::Lmstudio | Provider::Custom { .. }, Some(v)) => {
                if !v.is_null()
                    && !matches!(&v, serde_yaml::Value::Mapping(m) if m.is_empty())
                {
                    return Err(AgentConfigError::UnknownFeatureKey {
                        provider: "ollama|lmstudio|custom".to_string(),
                        key: "(any)".to_string(),
                    });
                }
                ProviderFeatures::None
            }
            (_, None) => match self.provider {
                Provider::Anthropic => ProviderFeatures::Anthropic(Default::default()),
                Provider::Openai => ProviderFeatures::OpenAI(Default::default()),
                Provider::Google => ProviderFeatures::Google(Default::default()),
                _ => ProviderFeatures::None,
            },
        };
        Ok(Binding {
            provider: self.provider,
            model: self.model,
            features,
        })
    }
}

fn feature_error(provider: &str, e: serde_yaml::Error) -> AgentConfigError {
    let msg = e.to_string();
    // serde_yaml's deny_unknown_fields error includes the offending key
    // verbatim — we surface the whole message rather than re-parsing it.
    AgentConfigError::UnknownFeatureKey {
        provider: provider.to_string(),
        key: msg,
    }
}

// ── override key ────────────────────────────────────────────────────────────

/// YAML key in the `overrides:` map. One of `<affinity>-<tier>`,
/// `<affinity>`, or `<tier>`. Parsed strictly — `affinity-only` collides
/// with `affinity-tier` only when both segments parse cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OverrideKey {
    pub affinity: Option<Affinity>,
    pub tier: Option<Tier>,
}

impl OverrideKey {
    pub fn parse(raw: &str) -> Result<Self, AgentConfigError> {
        // Try affinity-tier first (the only form with `-` between two
        // closed-enum members — `web-search` is itself hyphenated, so we
        // look for the LAST `-` and try both halves).
        if let Some(idx) = raw.rfind('-') {
            let (left, right) = (&raw[..idx], &raw[idx + 1..]);
            if let (Ok(a), Ok(t)) = (Affinity::from_str(left), Tier::from_str(right)) {
                return Ok(OverrideKey {
                    affinity: Some(a),
                    tier: Some(t),
                });
            }
        }
        if let Ok(a) = Affinity::from_str(raw) {
            return Ok(OverrideKey {
                affinity: Some(a),
                tier: None,
            });
        }
        if let Ok(t) = Tier::from_str(raw) {
            return Ok(OverrideKey {
                affinity: None,
                tier: Some(t),
            });
        }
        Err(AgentConfigError::UnknownOverrideKey(raw.to_string()))
    }
}

impl fmt::Display for OverrideKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.affinity, self.tier) {
            (Some(a), Some(t)) => write!(f, "{a}-{t}"),
            (Some(a), None) => write!(f, "{a}"),
            (None, Some(t)) => write!(f, "{t}"),
            (None, None) => f.write_str("default"),
        }
    }
}

impl<'de> Deserialize<'de> for OverrideKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        OverrideKey::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ── top-level file ──────────────────────────────────────────────────────────

/// `agents.yaml` on-disk shape. Mandatory: `version`, `default`. Optional:
/// `strict_specificity`, `overrides`.
///
/// FMECA U4 mitigation: `default` has NO `#[serde(default)]`. Missing →
/// load error.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentsFile {
    version: u8,
    #[serde(default)]
    strict_specificity: bool,
    default: Vec<RawBinding>,
    #[serde(default)]
    overrides: BTreeMap<OverrideKey, Vec<RawBinding>>,
}

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub version: u8,
    pub strict_specificity: bool,
    pub default: Vec<Binding>,
    pub overrides: BTreeMap<OverrideKey, Vec<Binding>>,
}

/// Forward-compat: the loader accepts only version 1. Higher versions
/// surface explicitly so an older flowgate against a newer config gives
/// a clear "upgrade" message instead of silently mis-parsing.
pub const CURRENT_AGENTS_FILE_VERSION: u8 = 1;

impl AgentsFile {
    /// Parse from a YAML string. Returns the typed in-memory shape;
    /// every High-risk FMECA row's check fires here at load time.
    pub fn from_yaml(input: &str) -> Result<Self, AgentConfigError> {
        let raw: RawAgentsFile = serde_yaml::from_str(input)
            .map_err(|e| AgentConfigError::YamlSyntax(e).refine_missing_default())?;
        if raw.version != CURRENT_AGENTS_FILE_VERSION {
            return Err(AgentConfigError::VersionMismatch {
                got: raw.version,
                expected: CURRENT_AGENTS_FILE_VERSION,
            });
        }
        if raw.default.is_empty() {
            // serde succeeded (the field was present) but the list is
            // empty — operator wrote `default: []`. Treat as missing-by-
            // intent: an empty default cannot resolve anything.
            return Err(AgentConfigError::EmptyDefault);
        }
        let default = raw
            .default
            .into_iter()
            .map(RawBinding::into_binding)
            .collect::<Result<Vec<_>, _>>()?;
        let mut overrides = BTreeMap::new();
        for (k, v) in raw.overrides {
            let bindings = v
                .into_iter()
                .map(RawBinding::into_binding)
                .collect::<Result<Vec<_>, _>>()?;
            overrides.insert(k, bindings);
        }
        Ok(AgentsFile {
            version: raw.version,
            strict_specificity: raw.strict_specificity,
            default,
            overrides,
        })
    }

    /// Convenience wrapper: read a file from disk.
    pub fn from_path(path: &Path) -> Result<Self, AgentConfigError> {
        let bytes = std::fs::read_to_string(path).map_err(AgentConfigError::Io)?;
        AgentsFile::from_yaml(&bytes)
    }
}

// ── error type ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AgentConfigError {
    #[error("agents.yaml is missing required `default:` section")]
    MissingDefault,

    #[error("agents.yaml `default:` is present but empty — at least one binding is required")]
    EmptyDefault,

    #[error("binding is missing `provider` and/or `model`")]
    MissingProviderModel,

    #[error(
        "agents.yaml override key `{0}` is not a valid <affinity> | <tier> | <affinity>-<tier>; \
         affinity ∈ {{coding, reasoning, prose, web-search, recon}}, \
         tier ∈ {{frontier, standard, commoditized}}"
    )]
    UnknownOverrideKey(String),

    #[error("provider `{provider}` rejected feature key(s): {key}")]
    UnknownFeatureKey { provider: String, key: String },

    #[error("provider `custom` requires a non-empty `endpoint` field")]
    ProviderEndpointRequired,

    #[error(
        "agents.yaml version mismatch: got {got}, this flowgate supports {expected}. \
         Upgrade flowgate or downgrade the config."
    )]
    VersionMismatch { got: u8, expected: u8 },

    #[error("agents.yaml syntax error: {0}")]
    YamlSyntax(#[source] serde_yaml::Error),

    #[error("agents.yaml I/O error: {0}")]
    Io(#[source] std::io::Error),
}

// Serde's `missing field "default"` error is a `serde_yaml::Error`; we
// translate it to the more specific `MissingDefault` variant at the call
// site that constructs `AgentsFile`. Done in `from_yaml` above via an
// inspection of the error message — but tests rely on the exact variant.
// Implement a translation helper:
impl AgentConfigError {
    /// Inspect a `YamlSyntax` error and re-extract the typed inner variant
    /// when serde's wrapping has lost it. Idempotent — pass-through when
    /// the inner cause isn't recognized.
    ///
    /// Why: custom deserializers (e.g. `OverrideKey::deserialize`) emit
    /// our typed errors via `serde::de::Error::custom`, which serde wraps
    /// in its own error chain. The string survives intact; the typed
    /// variant doesn't. This refiner reconstructs the variant by matching
    /// the stable marker strings embedded in each variant's `Display`
    /// impl. Tests in `agent_resolver_config.rs` pin the marker strings.
    pub fn refine_missing_default(self) -> Self {
        if let AgentConfigError::YamlSyntax(e) = &self {
            let msg = e.to_string();
            if msg.contains("missing field `default`") {
                return AgentConfigError::MissingDefault;
            }
            // `OverrideKey::parse` emits its key in the form:
            // ``agents.yaml override key `<KEY>` is not a valid``.
            if let Some(key) = extract_between(&msg, "override key `", "` is not a valid") {
                return AgentConfigError::UnknownOverrideKey(key.to_string());
            }
        }
        self
    }
}

fn extract_between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let s = haystack.find(start)? + start.len();
    let rest = &haystack[s..];
    let e = rest.find(end)?;
    Some(&rest[..e])
}
