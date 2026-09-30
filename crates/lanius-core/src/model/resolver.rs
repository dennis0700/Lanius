//! Resolves the model name a client requests to a concrete Kiro model id.
//!
//! [`ModelResolver::resolve`] is the main entry point, consulted by the API
//! route handlers in [`crate::api`] for every request. It applies, in
//! order: an exact-match alias table (configured externally-facing names
//! mapped to internal ones), then [`normalize_model_name`] to canonicalize
//! whatever spelling/versioning convention the client used, then checks the
//! live catalog in [`super::ModelInfoCache`], then a
//! hidden-model mapping, and finally falls back to passing the normalized
//! name through unverified. [`normalize_model_name`] itself handles the
//! several distinct naming conventions Anthropic/Kiro have used for Claude
//! model ids over time (see its doc comment for the full list).
//! [`fetch_available_models`] fetches the live catalog from Kiro used to
//! populate [`super::ModelInfoCache`] in the first place.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use super::cache::ModelInfoCache;
use super::reasoning::returns_visible_thinking;
use crate::auth::AuthManager;
use crate::config::{Config, default_hidden_from_list, default_model_aliases};
use crate::upstream::KiroHttpClient;

/// Result of resolving a client-supplied model name to a concrete Kiro model
/// id, as returned by [`ModelResolver::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResolution {
    /// The id to actually send to Kiro.
    pub internal_id: String,
    /// Which resolution path produced `internal_id`: `"cache"` (found in the
    /// live/fallback catalog), `"hidden"` (matched a hidden-model mapping),
    /// or `"passthrough"` (unverified; normalized name used as-is).
    pub source: &'static str,
    /// The exact model name as the client sent it, before alias/normalization.
    pub original_request: String,
    /// The name after alias substitution and [`normalize_model_name`].
    pub normalized: String,
    /// Whether `internal_id` is known to be valid (cache or hidden-model
    /// match) as opposed to an unverified passthrough guess.
    pub is_verified: bool,
}

/// Public-facing details about an available model, as returned by
/// [`ModelResolver::get_available_model_details`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDetails {
    /// The model's client-facing id.
    pub id: String,
    /// Human-readable description, when the catalog provides one.
    pub description: Option<String>,
    /// Kiro credit-usage rate multiplier relative to a baseline model, when
    /// the catalog provides one.
    pub rate_multiplier: Option<f64>,
    /// Whether the model returns visible native thinking (see
    /// [`crate::model::returns_visible_thinking`]). GPT-style models accept a
    /// reasoning effort but Kiro never returns their reasoning text, so they
    /// report `false`.
    pub supports_thinking: bool,
}

/// Resolves client-facing model names to Kiro model ids using a shared model
/// catalog cache, an alias table, a hidden-model mapping, and a
/// hide-from-listing set. Cheap to clone (all fields are cheaply-cloneable
/// or reference-counted), so a single resolver instance is typically shared
/// via [`crate::server::AppState::model_resolver`].
#[derive(Debug, Clone)]
pub struct ModelResolver {
    cache: ModelInfoCache,
    hidden_models: HashMap<String, String>,
    aliases: HashMap<String, String>,
    hidden_from_list: HashSet<String>,
}

impl ModelResolver {
    /// Creates a resolver from its full set of inputs directly, mainly useful
    /// in tests; production code typically uses [`ModelResolver::from_config`].
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use std::collections::HashMap;
    ///
    /// let resolver = ModelResolver::new(
    ///     ModelInfoCache::default(),
    ///     HashMap::new(),
    ///     HashMap::new(),
    ///     Vec::new(),
    /// );
    /// assert!(resolver.get_available_models().is_empty());
    /// ```
    pub fn new(
        cache: ModelInfoCache,
        hidden_models: HashMap<String, String>,
        aliases: HashMap<String, String>,
        hidden_from_list: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            cache,
            hidden_models,
            aliases,
            hidden_from_list: hidden_from_list.into_iter().collect(),
        }
    }

    /// Creates a resolver using the alias table and hidden-from-list
    /// configured in `config`, with no hidden-model mapping. This is the
    /// standard constructor used by `crate::server::AppState::initialize`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use lanius_core::Config;
    ///
    /// let resolver = ModelResolver::from_config(ModelInfoCache::default(), &Config::default());
    /// // The default alias table maps "auto-kiro" to "auto".
    /// assert_eq!(resolver.resolve("auto-kiro").internal_id, "auto");
    /// ```
    pub fn from_config(cache: ModelInfoCache, config: &Config) -> Self {
        Self::new(
            cache,
            HashMap::new(),
            config.model_aliases.clone(),
            config.hidden_from_list.clone(),
        )
    }

    /// Creates a resolver using the built-in default alias table and
    /// hidden-from-list ([`default_model_aliases`], [`default_hidden_from_list`]),
    /// with no hidden-model mapping. Mainly used in tests and standalone tools.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    ///
    /// let resolver = ModelResolver::with_defaults(ModelInfoCache::default());
    /// assert_eq!(resolver.resolve("auto-kiro").internal_id, "auto");
    /// ```
    pub fn with_defaults(cache: ModelInfoCache) -> Self {
        Self::new(
            cache,
            HashMap::new(),
            default_model_aliases(),
            default_hidden_from_list(),
        )
    }

    /// Resolves `external_model` (the model name a client requested) to a
    /// [`ModelResolution`]. Resolution order: exact alias match first (case
    /// sensitive), then [`normalize_model_name`] on the (possibly aliased)
    /// name, then a cache lookup, then a hidden-model lookup, and finally an
    /// unverified passthrough of the normalized name.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4.5"})]);
    /// let resolver = ModelResolver::with_defaults(cache);
    /// let resolution = resolver.resolve("claude-sonnet-4-5");
    /// assert_eq!(resolution.internal_id, "claude-sonnet-4.5");
    /// assert!(resolution.is_verified);
    /// ```
    pub fn resolve(&self, external_model: &str) -> ModelResolution {
        let resolved_model = self
            .aliases
            .get(external_model)
            .map_or(external_model, String::as_str);
        let normalized = normalize_model_name(resolved_model);

        if self.cache.is_valid_model(&normalized) {
            return ModelResolution {
                internal_id: to_runtime_model_id(&normalized),
                source: "cache",
                original_request: external_model.to_string(),
                normalized,
                is_verified: true,
            };
        }

        if let Some(internal_id) = self.hidden_models.get(&normalized) {
            return ModelResolution {
                internal_id: to_runtime_model_id(internal_id),
                source: "hidden",
                original_request: external_model.to_string(),
                normalized,
                is_verified: true,
            };
        }

        ModelResolution {
            internal_id: to_runtime_model_id(&normalized),
            source: "passthrough",
            original_request: external_model.to_string(),
            normalized,
            is_verified: false,
        }
    }

    /// Returns the sorted list of model ids that should be advertised to
    /// clients: the cached catalog plus hidden-model ids, minus anything in
    /// the hide-from-list, plus all configured alias names (which are always
    /// shown even if their target is hidden).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4.5"})]);
    /// let resolver = ModelResolver::with_defaults(cache);
    /// assert!(resolver.get_available_models().contains(&"claude-sonnet-4.5".to_string()));
    /// ```
    pub fn get_available_models(&self) -> Vec<String> {
        let mut models: HashSet<String> = self.cache.get_all_model_ids().into_iter().collect();
        models.extend(self.hidden_models.keys().cloned());
        models.retain(|model| !self.hidden_from_list.contains(model));
        models.extend(self.aliases.keys().cloned());
        let mut models: Vec<String> = models.into_iter().collect();
        models.sort();
        models
    }

    /// Like [`get_available_models`](Self::get_available_models), but
    /// enriched with each model's cached `description` and `rateMultiplier`
    /// where available (alias-only entries with no matching cache row get
    /// `None` for both), plus whether the model (or an alias's target)
    /// returns visible native thinking.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use serde_json::json;
    /// use std::collections::HashMap;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "m", "description": "A model", "rateMultiplier": 1.0})]);
    /// let resolver = ModelResolver::new(cache, HashMap::new(), HashMap::new(), Vec::new());
    /// let details = resolver.get_available_model_details();
    /// assert_eq!(details[0].description.as_deref(), Some("A model"));
    /// ```
    pub fn get_available_model_details(&self) -> Vec<ModelDetails> {
        self.get_available_models()
            .into_iter()
            .map(|id| {
                let cached = self.cache.get(&id);
                let description = cached
                    .as_ref()
                    .and_then(|model| model.get("description"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let rate_multiplier = cached
                    .as_ref()
                    .and_then(|model| model.get("rateMultiplier"))
                    .and_then(|value| value.as_f64());
                let internal_id = self.resolve(&id).internal_id;
                let supports_thinking = returns_visible_thinking(
                    &internal_id,
                    self.cache.reasoning_capability(&internal_id).as_ref(),
                );
                ModelDetails {
                    id,
                    description,
                    rate_multiplier,
                    supports_thinking,
                }
            })
            .collect()
    }

    /// Filters [`get_available_models`](Self::get_available_models) to ids
    /// whose lower-cased form contains `family` (e.g. `"haiku"`, `"sonnet"`,
    /// `"opus"`) as a substring.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4.5"}), json!({"modelId": "claude-opus-4.5"})]);
    /// let resolver = ModelResolver::with_defaults(cache);
    /// assert_eq!(resolver.get_models_by_family("sonnet"), vec!["claude-sonnet-4.5".to_string()]);
    /// ```
    pub fn get_models_by_family(&self, family: &str) -> Vec<String> {
        let family = family.to_ascii_lowercase();
        self.get_available_models()
            .into_iter()
            .filter(|model| model.to_ascii_lowercase().contains(&family))
            .collect()
    }

    /// Suggests alternative model names for an invalid/unknown request:
    /// models from the same Claude family (haiku/sonnet/opus) if one can be
    /// detected in `model_name`, otherwise the full list of available
    /// models. Used to build "did you mean...?" error messages.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4.5"})]);
    /// let resolver = ModelResolver::with_defaults(cache);
    /// let suggestions = resolver.get_suggestions_for_model("claude-sonnet-99");
    /// assert!(suggestions.contains(&"claude-sonnet-4.5".to_string()));
    /// ```
    pub fn get_suggestions_for_model(&self, model_name: &str) -> Vec<String> {
        extract_model_family(model_name)
            .map(|family| self.get_models_by_family(family))
            .unwrap_or_else(|| self.get_available_models())
    }

    /// Access to the underlying model catalog cache, for callers that need
    /// catalog data beyond what this resolver exposes directly.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::{ModelInfoCache, ModelResolver};
    ///
    /// let resolver = ModelResolver::with_defaults(ModelInfoCache::default());
    /// assert!(resolver.cache().is_empty());
    /// ```
    pub fn cache(&self) -> &ModelInfoCache {
        &self.cache
    }
}

/// Converts a normalized model name into the id actually sent to Kiro.
/// Currently an identity mapping (Kiro's runtime ids match the normalized
/// external names), kept as its own function so a future divergence between
/// "normalized" and "runtime" naming doesn't require touching every call site.
///
/// # Examples
///
/// ```
/// use lanius_core::model::to_runtime_model_id;
///
/// assert_eq!(to_runtime_model_id("claude-sonnet-4.5"), "claude-sonnet-4.5");
/// ```
pub fn to_runtime_model_id(normalized: &str) -> String {
    normalized.to_string()
}

/// Calls Kiro's `ListAvailableModels` control-plane API to fetch the
/// configured account's model catalog. Returns `None` (rather than
/// propagating an error) on any failure — token fetch, request
/// construction, network error, non-success HTTP status, or unparseable
/// body — so callers can uniformly fall back to the built-in model list.
///
/// # Examples
///
/// ```no_run
/// use lanius_core::auth::AuthManager;
/// use lanius_core::model::fetch_available_models;
/// use lanius_core::Config;
/// use std::sync::Arc;
///
/// # async fn example() {
/// let config = Config::default();
/// let auth = Arc::new(AuthManager::new(config.clone()).expect("valid config"));
/// let models = fetch_available_models(auth, &config).await;
/// # }
/// ```
pub async fn fetch_available_models(auth: Arc<AuthManager>, config: &Config) -> Option<Vec<Value>> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "origin".to_owned(),
        Value::String(crate::utils::KIRO_ORIGIN.to_owned()),
    );
    // Kiro CLI traffic is served by the Kiro control plane, which rejects
    // requests without a profile ARN whenever one exists for the account.
    if let Some(profile_arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) {
        payload.insert("profileArn".to_owned(), Value::String(profile_arn));
    }
    let body = serde_json::to_vec(&Value::Object(payload)).ok()?;

    let token = auth.access_token().await.ok()?;
    let client = KiroHttpClient::new(auth.clone(), config).ok()?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in crate::utils::kiro_headers_for(&token, crate::utils::API_RUNTIME) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    headers.insert(
        "x-amz-target",
        reqwest::header::HeaderValue::from_static("AmazonCodeWhispererService.ListAvailableModels"),
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json; charset=UTF-8"),
    );
    headers.insert(
        reqwest::header::CONTENT_ENCODING,
        reqwest::header::HeaderValue::from_static("amz-1.0"),
    );

    let url = auth.control_plane_host().await;
    let mut request = client.client().post(url).body(body);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request.send().await.ok()?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(%status, "ListAvailableModels returned a non-success status");
        return None;
    }
    let response = response.json::<Value>().await.ok()?;
    catalog_models(response)
}

/// Extracts the `models` array from a `ListAvailableModels` response body.
fn catalog_models(mut response: Value) -> Option<Vec<Value>> {
    match response.get_mut("models")?.take() {
        Value::Array(models) => Some(models),
        _ => None,
    }
}

/// Canonicalizes a Claude model name into a single stable form
/// (`claude-<family>-<major>.<minor>`, e.g. `claude-sonnet-4.5`), absorbing
/// the several distinct spelling conventions that Anthropic/Kiro clients
/// have used for the same model:
///
/// - **Standard, with minor version**: `claude-<family>-<major>-<minor>[-<date|"latest">]`
///   (e.g. `claude-sonnet-4-5-20250929` or `claude-opus-4-5-latest`).
/// - **Standard, no minor version**: `claude-<family>-<major>[-<8-digit date>]`
///   (e.g. `claude-sonnet-4-20250514`).
/// - **Legacy dash-separated**: `claude-<major>-<minor>-<family>[-<date|"latest">]`
///   (e.g. `claude-3-7-sonnet`).
/// - **Dotted with trailing date**: `claude-<family>-<major>.<minor>-<8-digit date>`
///   or `claude-<major>.<minor>-<family>-<8-digit date>`
///   (e.g. `claude-haiku-4.5-20251001`).
/// - **Inverted with a trailing modifier**: `claude-<major>.<minor>-<family>-<suffix...>`
///   (e.g. `claude-4.5-opus-high`, where `high`/`low-thinking` etc. are
///   dropped).
///
/// A model name already in canonical form, or one that matches none of the
/// above (e.g. `gpt-4`, alias names), is returned unchanged (aside from an
/// optional trailing `[<n><unit>]` context-size suffix, which is always
/// stripped first via `strip_context_suffix`). Matching happens on a
/// lower-cased copy, but the *original* casing is what gets returned when no
/// pattern matches — so unrecognized names round-trip byte-for-byte.
///
/// # Examples
///
/// ```
/// use lanius_core::model::normalize_model_name;
///
/// assert_eq!(normalize_model_name("claude-sonnet-4-5"), "claude-sonnet-4.5");
/// assert_eq!(normalize_model_name("claude-3-7-sonnet"), "claude-3.7-sonnet");
/// assert_eq!(normalize_model_name("gpt-4"), "gpt-4");
/// ```
pub fn normalize_model_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let without_context_suffix = strip_context_suffix(name);
    let lowercase = without_context_suffix.to_ascii_lowercase();
    let segments: Vec<&str> = lowercase.split('-').collect();

    if let Some(normalized) = normalize_standard_with_minor(&segments) {
        return normalized;
    }
    if let Some(normalized) = normalize_standard_without_minor(&segments) {
        return normalized;
    }
    if let Some(normalized) = normalize_legacy(&segments) {
        return normalized;
    }
    if let Some(normalized) = normalize_dotted_with_date(&segments) {
        return normalized;
    }
    if let Some(normalized) = normalize_inverted_with_suffix(&segments) {
        return normalized;
    }

    without_context_suffix.to_string()
}

/// Normalizes `model_name` and, if it matches an entry in `hidden_models`,
/// returns the mapped internal id instead of the normalized name.
/// Standalone helper for callers (e.g. converters) that have their own
/// hidden-model map and don't need a full [`ModelResolver`].
///
/// # Examples
///
/// ```
/// use lanius_core::model::get_model_id_for_kiro;
/// use std::collections::HashMap;
///
/// let hidden = HashMap::from([("claude-3.7-sonnet".to_string(), "INTERNAL_ID".to_string())]);
/// assert_eq!(get_model_id_for_kiro("claude-3-7-sonnet", &hidden), "INTERNAL_ID");
/// assert_eq!(get_model_id_for_kiro("unknown", &hidden), "unknown");
/// ```
pub fn get_model_id_for_kiro(model_name: &str, hidden_models: &HashMap<String, String>) -> String {
    let normalized = normalize_model_name(model_name);
    hidden_models
        .get(&normalized)
        .map_or(normalized, |internal_id| to_runtime_model_id(internal_id))
}

/// Finds the earliest-occurring Claude family keyword (`"haiku"`,
/// `"sonnet"`, or `"opus"`) within `model_name` (case-insensitive), used to
/// scope "did you mean...?" suggestions to the same family.
///
/// # Examples
///
/// ```
/// use lanius_core::model::extract_model_family;
///
/// assert_eq!(extract_model_family("claude-sonnet-4.5"), Some("sonnet"));
/// assert_eq!(extract_model_family("gpt-4"), None);
/// ```
pub fn extract_model_family(model_name: &str) -> Option<&'static str> {
    let lowercase = model_name.to_ascii_lowercase();
    ["haiku", "sonnet", "opus"]
        .into_iter()
        .filter_map(|family| lowercase.find(family).map(|position| (position, family)))
        .min_by_key(|(position, _)| *position)
        .map(|(_, family)| family)
}

// Strips a trailing `[<digits><unit>]` context-window annotation (e.g.
// `[200k]`, `[1M]`) some clients append to a model name, since it is
// metadata about the request rather than part of the model id itself.
// Returns `name` unchanged if the trailing bracket isn't in exactly this
// shape (digits followed by a single k/K/m/M unit character).
fn strip_context_suffix(name: &str) -> &str {
    let Some(opening) = name.rfind('[') else {
        return name;
    };
    let Some(suffix) = name.get(opening..) else {
        return name;
    };
    let mut chars = suffix.chars();
    if chars.next() != Some('[') || !suffix.ends_with(']') {
        return name;
    }
    let body = &suffix[1..suffix.len().saturating_sub(1)];
    let mut body_chars = body.chars();
    let Some(unit) = body_chars.next_back() else {
        return name;
    };
    if matches!(unit, 'm' | 'M' | 'k' | 'K')
        && body_chars.all(|character| character.is_ascii_digit())
    {
        name.get(..opening).unwrap_or(name)
    } else {
        name
    }
}

// Any purely alphabetic segment counts as a family name, so new Claude
// families (e.g. `fable`) normalize without code changes.
fn is_family(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_lowercase())
}

fn is_version(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
}

fn is_suffix(value: &str) -> bool {
    value == "latest" || is_version(value)
}

// Matches `claude-<family>-<major>-<minor>[-<date|"latest">]` where `minor`
// is 1-2 digits (e.g. `claude-sonnet-4-5-20250929`, `claude-opus-4-5-latest`).
// The optional 5th segment may be a release date or the literal "latest";
// anything else in that position fails the match.
fn normalize_standard_with_minor(segments: &[&str]) -> Option<String> {
    if !(segments.len() == 4 || segments.len() == 5) {
        return None;
    }
    let ["claude", family, major, minor, ..] = segments else {
        return None;
    };
    if !is_family(family)
        || !is_version(major)
        || !(1..=2).contains(&minor.len())
        || !is_version(minor)
        || (segments.len() == 5 && !is_suffix(segments[4]))
    {
        return None;
    }
    Some(format!("claude-{family}-{major}.{minor}"))
}

// Matches `claude-<family>-<major>[-<8-digit date>]` with no minor version
// segment (e.g. `claude-sonnet-4-20250514`). The optional trailing segment
// must be exactly 8 digits (a YYYYMMDD release date), not "latest".
fn normalize_standard_without_minor(segments: &[&str]) -> Option<String> {
    if !(segments.len() == 3 || segments.len() == 4) {
        return None;
    }
    let ["claude", family, major, ..] = segments else {
        return None;
    };
    if !is_family(family)
        || !is_version(major)
        || (segments.len() == 4 && !(segments[3].len() == 8 && is_version(segments[3])))
    {
        return None;
    }
    Some(format!("claude-{family}-{major}"))
}

// Matches the older `claude-<major>-<minor>-<family>[-<date|"latest">]`
// ordering, where the version segments come before the family name (e.g.
// `claude-3-7-sonnet`).
fn normalize_legacy(segments: &[&str]) -> Option<String> {
    if !(segments.len() == 4 || segments.len() == 5) {
        return None;
    }
    let ["claude", major, minor, family, ..] = segments else {
        return None;
    };
    if !is_version(major)
        || !is_version(minor)
        || !is_family(family)
        || (segments.len() == 5 && !is_suffix(segments[4]))
    {
        return None;
    }
    Some(format!("claude-{major}.{minor}-{family}"))
}

// Matches a name that already contains a dotted version (`major.minor`)
// together with a trailing 8-digit release date, in either
// `claude-<family>-<major.minor>-<date>` or `claude-<major.minor>-<family>-<date>`
// order (e.g. `claude-haiku-4.5-20251001`). Both orderings are checked and
// whichever one has a valid family/version split wins; if the trailing date
// segment isn't an 8-digit number this doesn't match at all.
fn normalize_dotted_with_date(segments: &[&str]) -> Option<String> {
    let (date, prefix) = segments.split_last()?;
    if date.len() != 8 || !is_version(date) {
        return None;
    }
    let prefix = prefix.join("-");
    let matches = prefix.strip_prefix("claude-").is_some_and(|suffix| {
        let standard = suffix
            .split_once('-')
            .is_some_and(|(family, version)| is_family(family) && dotted_version(version));
        let legacy = suffix
            .split_once('-')
            .is_some_and(|(version, family)| dotted_version(version) && is_family(family));
        standard || legacy
    });
    matches.then_some(prefix)
}

fn dotted_version(value: &str) -> bool {
    let Some((major, minor)) = value.split_once('.') else {
        return false;
    };
    is_version(major) && is_version(minor)
}

// Matches `claude-<major.minor>-<family>-<suffix...>` where the version
// comes before the family and is followed by one or more trailing modifier
// segments (e.g. `claude-4.5-opus-high`, `claude-4.5-sonnet-low-thinking`).
// The suffix segments themselves are discarded; only the family/version are
// kept in the normalized output. Requires at least one suffix segment to
// avoid overlapping with `normalize_dotted_with_date`'s date-suffix case.
fn normalize_inverted_with_suffix(segments: &[&str]) -> Option<String> {
    let ["claude", version, family, suffix @ ..] = segments else {
        return None;
    };
    if suffix.is_empty() || !is_family(family) || !dotted_version(version) {
        return None;
    }
    let (major, minor) = version.split_once('.')?;
    Some(format!("claude-{family}-{major}.{minor}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cache() -> ModelInfoCache {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId":"auto"}),
            json!({"modelId":"claude-sonnet-4"}),
            json!({"modelId":"claude-sonnet-4.5"}),
            json!({"modelId":"claude-haiku-4.5"}),
            json!({"modelId":"claude-opus-4.5"}),
        ]);
        cache
    }

    fn resolver() -> ModelResolver {
        ModelResolver::new(
            cache(),
            HashMap::from([(
                "claude-3.7-sonnet".to_string(),
                "CLAUDE_3_7_SONNET_20250219_V1_0".to_string(),
            )]),
            HashMap::new(),
            Vec::new(),
        )
    }

    #[test]
    fn normalization_mapping_table_is_complete() {
        for (input, expected) in [
            ("claude-haiku-4-5", "claude-haiku-4.5"),
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
            ("claude-opus-4-5-latest", "claude-opus-4.5"),
            ("claude-sonnet-4-20250514", "claude-sonnet-4"),
            ("claude-3-7-sonnet", "claude-3.7-sonnet"),
            ("claude-3-5-haiku-20250219", "claude-3.5-haiku"),
            ("claude-3-0-opus", "claude-3.0-opus"),
            ("claude-haiku-4.5-20251001", "claude-haiku-4.5"),
            ("claude-3.7-sonnet-20250219", "claude-3.7-sonnet"),
            ("claude-4.5-opus-high", "claude-opus-4.5"),
            ("claude-4.5-sonnet-low-thinking", "claude-sonnet-4.5"),
            ("claude-4.5-haiku-high", "claude-haiku-4.5"),
            ("CLAUDE-4.5-OPUS-HIGH", "claude-opus-4.5"),
            ("claude-haiku-4.5", "claude-haiku-4.5"),
            ("claude-4.5-sonnet", "claude-4.5-sonnet"),
            ("gpt-4", "gpt-4"),
            ("AUTO-KIRO", "AUTO-KIRO"),
            ("claude-sonnet-4-5[200k]", "claude-sonnet-4.5"),
            ("claude-fable-5-1", "claude-fable-5.1"),
            ("claude-fable-5", "claude-fable-5"),
            ("claude-5.1-fable-high", "claude-fable-5.1"),
            ("claude-instant-1", "claude-instant-1"),
        ] {
            assert_eq!(normalize_model_name(input), expected, "{input}");
        }
    }

    #[test]
    fn converter_helper_normalizes_then_uses_exact_hidden_mapping() {
        let hidden = HashMap::from([("claude-3.7-sonnet".to_string(), "INTERNAL".to_string())]);
        assert_eq!(
            get_model_id_for_kiro("claude-3-7-sonnet", &hidden),
            "INTERNAL"
        );
        assert_eq!(get_model_id_for_kiro("unknown", &hidden), "unknown");
    }

    #[test]
    fn resolve_uses_cache_hidden_and_passthrough_in_order() {
        let resolver = resolver();
        let cached = resolver.resolve("claude-haiku-4-5");
        assert_eq!(
            (
                cached.internal_id.as_str(),
                cached.source,
                cached.is_verified
            ),
            ("claude-haiku-4.5", "cache", true)
        );
        let hidden = resolver.resolve("claude-3-7-sonnet");
        assert_eq!(
            (
                hidden.internal_id.as_str(),
                hidden.source,
                hidden.is_verified
            ),
            ("CLAUDE_3_7_SONNET_20250219_V1_0", "hidden", true)
        );
        let unknown = resolver.resolve("claude-opus-5");
        assert_eq!(
            (
                unknown.internal_id.as_str(),
                unknown.source,
                unknown.is_verified
            ),
            ("claude-opus-5", "passthrough", false)
        );
    }

    #[test]
    fn supports_thinking_is_only_reported_for_visible_thinking() {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId": "claude-opus-5.5", "additionalModelRequestFieldsSchema": {"properties": {
                "thinking": {"properties": {"type": {"enum": ["adaptive"]}}}
            }}}),
            json!({"modelId": "gpt-5.6-luna", "additionalModelRequestFieldsSchema": {"properties": {
                "reasoning": {"properties": {"effort": {"enum": ["none", "high"]}}}
            }}}),
            json!({"modelId": "claude-sonnet-4.5"}),
            json!({"modelId": "minimax-m2.5"}),
        ]);
        let resolver = ModelResolver::new(cache, HashMap::new(), HashMap::new(), Vec::new());
        let flags: HashMap<String, bool> = resolver
            .get_available_model_details()
            .into_iter()
            .map(|details| (details.id, details.supports_thinking))
            .collect();
        assert!(flags["claude-opus-5.5"]);
        assert!(!flags["gpt-5.6-luna"]);
        assert!(!flags["claude-sonnet-4.5"]);
        assert!(
            flags["minimax-m2.5"],
            "schema-less models on the allowlist count"
        );
    }

    #[test]
    fn aliases_are_exact_case_sensitive_and_take_precedence() {
        let resolver = ModelResolver::new(
            cache(),
            HashMap::new(),
            HashMap::from([
                ("auto".to_string(), "claude-sonnet-4.5".to_string()),
                ("my-haiku".to_string(), "claude-haiku-4-5".to_string()),
            ]),
            Vec::new(),
        );
        assert_eq!(resolver.resolve("auto").internal_id, "claude-sonnet-4.5");
        assert_eq!(resolver.resolve("my-haiku").internal_id, "claude-haiku-4.5");
        assert_eq!(resolver.resolve("MY-HAIKU").internal_id, "MY-HAIKU");
    }

    #[test]
    fn model_listing_filters_hidden_ids_then_adds_aliases() {
        let resolver = ModelResolver::new(
            cache(),
            HashMap::from([("claude-3.7-sonnet".to_string(), "INTERNAL".to_string())]),
            HashMap::from([("auto-kiro".to_string(), "auto".to_string())]),
            vec!["auto".to_string()],
        );
        let models = resolver.get_available_models();
        assert!(!models.contains(&"auto".to_string()));
        assert!(models.contains(&"auto-kiro".to_string()));
        assert!(models.contains(&"claude-3.7-sonnet".to_string()));
        assert!(models.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn model_details_carry_cache_description_and_rate_multiplier() {
        let cache = ModelInfoCache::default();
        cache.update(vec![json!({
            "modelId": "claude-sonnet-4",
            "description": "Claude Sonnet 4 model with 1M context window",
            "rateMultiplier": 1.3,
        })]);
        let resolver = ModelResolver::new(cache, HashMap::new(), HashMap::new(), Vec::new());
        let details = resolver.get_available_model_details();
        let sonnet = details
            .iter()
            .find(|model| model.id == "claude-sonnet-4")
            .expect("cached model must be present");
        assert_eq!(
            sonnet.description.as_deref(),
            Some("Claude Sonnet 4 model with 1M context window")
        );
        assert_eq!(sonnet.rate_multiplier, Some(1.3));
    }

    #[test]
    fn model_details_default_to_none_when_cache_row_is_missing() {
        let resolver = ModelResolver::new(
            cache(),
            HashMap::new(),
            HashMap::from([("auto-kiro".to_string(), "auto".to_string())]),
            Vec::new(),
        );
        let details = resolver.get_available_model_details();
        let alias = details
            .iter()
            .find(|model| model.id == "auto-kiro")
            .expect("alias entry must be present");
        assert_eq!(alias.description, None);
        assert_eq!(alias.rate_multiplier, None);
    }

    #[test]
    fn family_suggestions_never_cross_model_families() {
        let resolver = resolver();
        assert_eq!(extract_model_family("CLAUDE-HAIKU-4.5"), Some("haiku"));
        assert_eq!(extract_model_family("gpt-4"), None);
        for family in ["haiku", "sonnet", "opus"] {
            assert!(
                resolver
                    .get_suggestions_for_model(&format!("claude-{family}-99"))
                    .iter()
                    .all(|model| model.to_ascii_lowercase().contains(family))
            );
        }
    }

    #[test]
    fn defaults_and_config_constructors_keep_cursor_auto_alias_behavior() {
        let defaults = ModelResolver::with_defaults(cache());
        assert_eq!(defaults.resolve("auto-kiro").internal_id, "auto");
        assert!(
            !defaults
                .get_available_models()
                .contains(&"auto".to_string())
        );
        let config = Config::default();
        let from_config = ModelResolver::from_config(cache(), &config);
        assert_eq!(from_config.resolve("auto-kiro").internal_id, "auto");
    }
}
