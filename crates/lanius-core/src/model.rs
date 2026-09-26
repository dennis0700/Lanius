//! Model catalog caching, model-name resolution/normalization, and native
//! reasoning capability detection.
//!
//! [`cache::ModelInfoCache`] holds the (periodically refreshed) Kiro model
//! catalog, and [`resolver::ModelResolver`] uses it — together with
//! configured aliases and hidden-model mappings — to turn the model name a
//! client requests into the concrete id sent upstream to Kiro.
//! [`reasoning`] reads each catalog entry's request-field schema to decide
//! whether (and how) a model supports native thinking/reasoning. This module
//! is consulted by both [`crate::server`] (to build shared state) and the
//! API route handlers in [`crate::api`] when resolving a request's `model`
//! field.

pub(crate) mod cache;
pub(crate) mod reasoning;
pub(crate) mod resolver;

pub use cache::ModelInfoCache;
pub use reasoning::{
    EffortLevel, ReasoningCapability, ReasoningProtocol, ReasoningRequest,
    THINKING_WITHOUT_SCHEMA, returns_visible_thinking,
};
pub use resolver::{
    ModelDetails, ModelResolution, ModelResolver, extract_model_family, fetch_available_models,
    get_model_id_for_kiro, normalize_model_name, to_runtime_model_id,
};
