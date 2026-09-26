//! Caches the Kiro model catalog (model ids and their metadata such as
//! context-window limits) in memory.
//!
//! [`ModelInfoCache`] is populated at startup from the built-in fallback
//! list ([`ModelInfoCache::load_fallback`]) and then periodically replaced
//! wholesale with the live catalog fetched from Kiro (see
//! [`crate::server::AppState::initialize`]). It is consulted by
//! [`crate::model::resolver::ModelResolver`] to validate/resolve model ids
//! and by [`crate::tokenizer::calculate_tokens_from_context_usage`] to look
//! up a model's context-window size.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{DEFAULT_MAX_INPUT_TOKENS, MODEL_CACHE_TTL, fallback_models};
use crate::model::reasoning::ReasoningCapability;

#[derive(Debug, Default)]
struct CacheState {
    models: HashMap<String, Value>,
    last_update: Option<Instant>,
}

/// Thread-safe, TTL-aware cache of the Kiro model catalog, keyed by model id.
/// Cloning shares the same underlying state (it wraps an
/// `Arc<RwLock<_>>`), so a single instance can live in shared server state
/// (see [`crate::server::AppState::model_cache`]) and be read/updated
/// concurrently across requests and the background refresh task.
#[derive(Debug, Clone)]
pub struct ModelInfoCache {
    state: Arc<RwLock<CacheState>>,
    cache_ttl: Duration,
}

impl Default for ModelInfoCache {
    fn default() -> Self {
        Self::new(MODEL_CACHE_TTL)
    }
}

impl ModelInfoCache {
    /// Creates an empty cache with a custom staleness TTL (see
    /// [`ModelInfoCache::is_stale`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use std::time::Duration;
    ///
    /// let cache = ModelInfoCache::new(Duration::from_secs(3600));
    /// assert!(cache.is_empty());
    /// ```
    pub fn new(cache_ttl: Duration) -> Self {
        Self {
            state: Arc::new(RwLock::new(CacheState::default())),
            cache_ttl,
        }
    }

    /// Replaces the entire cached catalog with `models_data`, keyed by each
    /// entry's `modelId` field (entries missing a `modelId` string are
    /// silently dropped) and resets the staleness clock. This is a full
    /// replace, not a merge: any model id not present in the new data is no
    /// longer considered valid afterward.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4"})]);
    /// assert!(cache.is_valid_model("claude-sonnet-4"));
    /// ```
    pub fn update<I>(&self, models_data: I)
    where
        I: IntoIterator<Item = Value>,
    {
        let models = models_data
            .into_iter()
            .filter_map(|model| {
                let model_id = model.get("modelId")?.as_str()?.to_string();
                Some((model_id, model))
            })
            .collect();
        let mut state = self.write_state();
        state.models = models;
        state.last_update = Some(Instant::now());
    }

    /// Returns the cached metadata JSON for `model_id`, if present.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4"})]);
    /// assert!(cache.get("claude-sonnet-4").is_some());
    /// assert!(cache.get("unknown").is_none());
    /// ```
    pub fn get(&self, model_id: &str) -> Option<Value> {
        self.read_state().models.get(model_id).cloned()
    }

    /// Seeds (replaces) the cache with the built-in [`fallback_models`]
    /// list, each with only a `modelId` field. Used at startup before the
    /// live catalog has been fetched, and again if a live refresh
    /// completely fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.load_fallback();
    /// assert!(!cache.is_empty());
    /// ```
    pub fn load_fallback(&self) {
        self.update(
            fallback_models()
                .into_iter()
                .map(|model_id| json!({"modelId": model_id})),
        );
    }

    /// Whether `model_id` exists in the cache, regardless of staleness.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "claude-sonnet-4"})]);
    /// assert!(cache.is_valid_model("claude-sonnet-4"));
    /// ```
    pub fn is_valid_model(&self, model_id: &str) -> bool {
        self.get(model_id).is_some()
    }

    /// Adds a synthetic entry for a model that should be resolvable by
    /// `display_name` but is not advertised by Kiro's public catalog,
    /// recording `internal_id` for traceability. A no-op if `display_name`
    /// is already present (first registration wins; re-adding the same
    /// display name with a different internal id has no effect).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.add_hidden_model("auto", "claude-sonnet-4");
    /// assert!(cache.is_valid_model("auto"));
    /// ```
    pub fn add_hidden_model(
        &self,
        display_name: impl Into<String>,
        internal_id: impl Into<String>,
    ) {
        let display_name = display_name.into();
        let internal_id = internal_id.into();
        let mut state = self.write_state();
        state.models.entry(display_name.clone()).or_insert_with(|| {
            json!({
                "modelId": display_name,
                "modelName": display_name,
                "description": format!("Hidden model (internal: {internal_id})"),
                "tokenLimits": {"maxInputTokens": DEFAULT_MAX_INPUT_TOKENS},
                "_internal_id": internal_id,
                "_is_hidden": true,
            })
        });
    }

    /// Returns the model's maximum input token count from cached metadata,
    /// falling back to [`DEFAULT_MAX_INPUT_TOKENS`] when the model is
    /// unknown, its limit is missing/non-positive, or it doesn't fit in a
    /// `u32`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "m", "tokenLimits": {"maxInputTokens": 123}})]);
    /// assert_eq!(cache.get_max_input_tokens("m"), 123);
    /// ```
    pub fn get_max_input_tokens(&self, model_id: &str) -> u32 {
        self.get(model_id)
            .and_then(|model| model.get("tokenLimits")?.get("maxInputTokens")?.as_u64())
            .and_then(|tokens| u32::try_from(tokens).ok())
            .filter(|tokens| *tokens > 0)
            .unwrap_or(DEFAULT_MAX_INPUT_TOKENS)
    }

    /// Returns the model's native thinking/reasoning capability, parsed from
    /// its cached `additionalModelRequestFieldsSchema`. `None` for unknown
    /// models and for models without native reasoning (including every
    /// entry of the built-in fallback list, which carries no schema).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![
    ///     json!({"modelId": "old"}),
    ///     json!({"modelId": "new", "additionalModelRequestFieldsSchema": {"properties": {
    ///         "thinking": {"properties": {"type": {"enum": ["adaptive"]}}}
    ///     }}}),
    /// ]);
    /// assert!(cache.reasoning_capability("old").is_none());
    /// assert!(cache.reasoning_capability("new").is_some());
    /// ```
    pub fn reasoning_capability(&self, model_id: &str) -> Option<ReasoningCapability> {
        self.read_state()
            .models
            .get(model_id)
            .and_then(ReasoningCapability::from_model)
    }

    /// Whether the cache currently holds no models at all.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    ///
    /// let cache = ModelInfoCache::default();
    /// assert!(cache.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.read_state().models.is_empty()
    }

    /// Whether the cache has never been populated, or was last populated
    /// longer ago than its configured TTL. Staleness does not clear the
    /// cache or block reads — callers typically use this only to decide
    /// whether to trigger a background refresh.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use std::time::Duration;
    ///
    /// let cache = ModelInfoCache::new(Duration::ZERO);
    /// assert!(cache.is_stale(), "an empty cache is always stale");
    /// ```
    pub fn is_stale(&self) -> bool {
        self.is_stale_state(&self.read_state())
    }

    /// Returns all cached model ids in unspecified order.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "m"})]);
    /// assert_eq!(cache.get_all_model_ids(), vec!["m".to_string()]);
    /// ```
    pub fn get_all_model_ids(&self) -> Vec<String> {
        self.read_state().models.keys().cloned().collect()
    }

    /// Number of models currently cached.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::model::ModelInfoCache;
    /// use serde_json::json;
    ///
    /// let cache = ModelInfoCache::default();
    /// cache.update(vec![json!({"modelId": "a"}), json!({"modelId": "b"})]);
    /// assert_eq!(cache.size(), 2);
    /// ```
    pub fn size(&self) -> usize {
        self.read_state().models.len()
    }

    fn is_stale_state(&self, state: &CacheState) -> bool {
        state
            .last_update
            .map(|updated| updated.elapsed() > self.cache_ttl)
            .unwrap_or(true)
    }

    // Acquires the read lock, recovering from poisoning (a prior panic while
    // holding the lock) rather than propagating a panic, since a poisoned
    // model cache is not worth crashing the whole gateway over.
    fn read_state(&self) -> RwLockReadGuard<'_, CacheState> {
        match self.state.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("model cache read lock was poisoned; recovering state");
                poisoned.into_inner()
            }
        }
    }

    // As above, for the write lock.
    fn write_state(&self) -> RwLockWriteGuard<'_, CacheState> {
        match self.state.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("model cache write lock was poisoned; recovering state");
                poisoned.into_inner()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<Value> {
        vec![
            json!({"modelId": "claude-sonnet-4", "tokenLimits": {"maxInputTokens": 200_000}}),
            json!({"modelId": "other", "tokenLimits": {"maxInputTokens": 50_000}}),
        ]
    }

    #[test]
    fn update_replaces_cache_and_gets_model_metadata() {
        let cache = ModelInfoCache::default();
        assert!(cache.is_empty());
        cache.update(models());
        assert!(!cache.is_empty());
        assert_eq!(cache.size(), 2);
        let model = cache.get("claude-sonnet-4");
        assert_eq!(
            model.as_ref().and_then(|v| v["modelId"].as_str()),
            Some("claude-sonnet-4")
        );
        cache.update(vec![json!({"modelId": "new"})]);
        assert_eq!(cache.size(), 1);
        assert!(cache.get("claude-sonnet-4").is_none());
    }

    #[test]
    fn max_input_tokens_uses_default_for_missing_or_invalid_values() {
        let cache = ModelInfoCache::default();
        cache.update(vec![
            json!({"modelId": "set", "tokenLimits": {"maxInputTokens": 123}}),
            json!({"modelId": "null", "tokenLimits": {"maxInputTokens": null}}),
            json!({"modelId": "missing"}),
        ]);
        assert_eq!(cache.get_max_input_tokens("set"), 123);
        assert_eq!(cache.get_max_input_tokens("null"), DEFAULT_MAX_INPUT_TOKENS);
        assert_eq!(
            cache.get_max_input_tokens("missing"),
            DEFAULT_MAX_INPUT_TOKENS
        );
        assert_eq!(
            cache.get_max_input_tokens("unknown"),
            DEFAULT_MAX_INPUT_TOKENS
        );
    }

    #[test]
    fn stale_cache_remains_readable_until_explicit_fallback_load() {
        let cache = ModelInfoCache::new(Duration::ZERO);
        assert!(cache.is_stale());
        assert!(cache.get_all_model_ids().is_empty());

        cache.update(vec![json!({"modelId": "dynamic"})]);
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.is_stale());
        assert_eq!(
            cache
                .get("dynamic")
                .as_ref()
                .and_then(|model| model["modelId"].as_str()),
            Some("dynamic")
        );
        assert!(cache.is_valid_model("dynamic"));
        assert_eq!(cache.get_all_model_ids(), vec!["dynamic".to_string()]);

        cache.load_fallback();
        assert!(cache.get("dynamic").is_none());
        for model_id in fallback_models() {
            assert!(cache.is_valid_model(model_id));
        }
    }

    #[test]
    fn hidden_model_is_added_once_with_default_limit() {
        let cache = ModelInfoCache::default();
        cache.add_hidden_model("hidden", "INTERNAL_A");
        cache.add_hidden_model("hidden", "INTERNAL_B");
        assert_eq!(cache.size(), 1);
        assert_eq!(
            cache.get_max_input_tokens("hidden"),
            DEFAULT_MAX_INPUT_TOKENS
        );
        let hidden = cache.get("hidden");
        assert_eq!(
            hidden
                .as_ref()
                .and_then(|value| value["_internal_id"].as_str()),
            Some("INTERNAL_A")
        );
    }

    #[test]
    fn clones_share_state_for_concurrent_request_handles() {
        let cache = ModelInfoCache::default();
        let shared = cache.clone();
        shared.update(vec![json!({"modelId": "shared"})]);
        assert!(cache.is_valid_model("shared"));
    }
}
