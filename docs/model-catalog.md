# Model Catalog and Resolution (`model`)

`model` has three parts:

- **`model::cache`** — `ModelInfoCache`, an in-memory TTL'd snapshot of
  Kiro's model catalog.
- **`model::resolver`** — `ModelResolver`, which turns whatever model name a
  client sends into a concrete Kiro model id.
- **`model::reasoning`** — `ReasoningCapability`, which derives a model's
  native thinking/reasoning support from its catalog schema.

## `ModelInfoCache`

A thread-safe (`Arc<RwLock<_>>`), cheaply-cloneable cache keyed by model id,
populated in two ways:

1. **Fallback** (`load_fallback`) — the built-in snapshot in
   `config::fallback_models()`, used at startup before any live fetch
   succeeds, and again if a later live fetch fails (the cache stays
   readable with stale data until an explicit fallback reload, rather than
   going empty on a transient fetch failure).
2. **Live** (`update`) — `crate::server::AppState::initialize` spawns a
   background task that periodically calls
   `model::resolver::fetch_available_models` (a `ListAvailableModels`
   request to Kiro's control-plane host) and calls `update`, which is a
   **full replace**, not a merge: any model id absent from the new data is
   no longer considered valid afterward.

`is_stale` reports whether the cache is older than `MODEL_CACHE_TTL` (1
hour by default), used by the server's own background refresh loop to
decide when to re-fetch. `get_max_input_tokens` falls back to
`DEFAULT_MAX_INPUT_TOKENS` (200,000) for models with an unknown or invalid
limit.

## `ModelResolver::resolve`

The main entry point, consulted by every API route handler for every
request. Applied in order:

1. **Alias table** (`config.model_aliases`) — exact-match, externally-facing
   names mapped to internal ids (e.g. `auto-kiro` -> `auto`).
2. **`normalize_model_name`** — canonicalizes whatever spelling/versioning
   convention the client used (see below).
3. **Live catalog check** (`ModelInfoCache::is_valid_model`) — if the
   normalized name matches a cached model, that's the answer
   (`source: "cache"`, `is_verified: true`).
4. **Hidden-model mapping** — a resolver-configured mapping for models kept
   resolvable but not advertised in `/v1/models` (`source: "hidden"`).
5. **Passthrough fallback** — if nothing matched, the normalized name is
   used unverified (`source: "passthrough"`, `is_verified: false`) rather
   than rejecting the request outright, so a request for a model Lanius
   hasn't cataloged yet still gets a chance to succeed against Kiro
   directly.

## Model name normalization

`normalize_model_name` canonicalizes Claude model names into one stable
form (`claude-<family>-<major>.<minor>`, e.g. `claude-sonnet-4.5`),
absorbing every spelling convention Anthropic/Kiro clients have used for
the same model over time:

| Convention | Example | Canonical |
|---|---|---|
| Standard, dash-separated minor | `claude-sonnet-4-5-20250929` | `claude-sonnet-4.5` |
| Standard, no minor | `claude-sonnet-4-20250514` | `claude-sonnet-4` |
| Legacy, family last | `claude-3-7-sonnet` | `claude-3.7-sonnet` |
| Dotted with trailing date | `claude-haiku-4.5-20251001` | `claude-haiku-4.5` |
| Inverted with trailing modifier | `claude-4.5-opus-high` | `claude-opus-4.5` |

Matching happens on a lower-cased copy; a name that already is canonical,
or matches none of the known patterns (e.g. `gpt-4`, an alias name), is
returned with its *original* casing so unrecognized names round-trip
byte-for-byte. A trailing `[<n><unit>]` context-size suffix (if present) is
always stripped first via `strip_context_suffix`.

## Advertised model list

`get_available_models` unions the live catalog's ids, the hidden-model
mapping's ids, and all configured alias names — minus anything in
`hidden_from_list` — then sorts the result. Alias names are always shown
even if their resolved target is hidden, since the alias is the name
clients are meant to use. `get_available_model_details` enriches this with
each model's cached `description`/`rateMultiplier` (`None` for alias-only
entries with no matching cache row) and `supports_thinking`.

## Native reasoning/thinking (`model::reasoning`)

Kiro's `ListAvailableModels` response describes, per model, which extra
request fields it accepts via `additionalModelRequestFieldsSchema`. Two
families expose native reasoning there:

- **Claude-style** (`ReasoningProtocol::Thinking`) — a `thinking` object
  (`type`: `adaptive`/`disabled`, `display`: `summarized`/`omitted`) plus
  an optional `output_config.effort` level.
- **GPT-style** (`ReasoningProtocol::Reasoning`) — a `reasoning.effort`
  level only.

Models with neither field get no reasoning fields and no thinking output at
all. `ReasoningCapability::from_model` parses the schema into a typed
capability (allowed effort levels, thinking types, display modes, default
effort); `ReasoningCapability::request_fields` then turns a client's
`ReasoningRequest` into the exact `additionalModelRequestFields` value sent
to Kiro, snapping a requested effort level to the *nearest* level the model
actually supports (`nearest_effort`) rather than rejecting unsupported
levels outright.

`EffortLevel` is a single ordered enum spanning both OpenAI's vocabulary
(`none`..`xhigh`) and Kiro's (`low`..`max`), so effort requested via either
client protocol maps onto the same scale before being snapped to what the
target model supports.

`returns_visible_thinking` determines whether a model's reasoning is
actually surfaced to the client as visible text (Claude-style models are;
GPT-style models accept a reasoning effort but Kiro never returns their
reasoning text, so they always report `false` here even though they do
"reason" internally). This is what `ModelDetails::supports_thinking` and
`OpenAIModel::supports_thinking` report to clients.
