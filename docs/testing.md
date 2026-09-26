# Testing

```sh
cargo test --workspace
```

runs everything: unit tests colocated with their modules, doctests on
public API examples, integration tests under each crate's `tests/`
directory, and property tests. There is no separate test command per
category — `cargo test --workspace` runs all of it.

## Where tests live

- **Unit tests** — nearly every module in `lanius-core` and `lanius-gui`
  has a `#[cfg(test)] mod tests` at the bottom of the file, testing that
  module's functions/types directly (including `pub(crate)`-only internals,
  which integration tests can't reach). This is the majority of the test
  suite.
- **Doctests** — most public functions/types carry a `# Examples` section
  with a runnable code block; `cargo test` compiles and runs these too.
  They double as usage documentation and as regression tests for the exact
  API signatures shown.
- **Integration tests** (`crates/*/tests/*.rs`) — test a crate's public API
  from outside, as an external consumer would:
  - `lanius-core/tests/tool_followup_payload.rs` — the tool-definition/
    tool-result alias round trip across a full request conversion (see
    [conversion.md](conversion.md) and
    [compatibility.md](compatibility.md#tool-name-aliasing)).
  - `lanius-core/tests/parser_properties.rs` — property-based tests (see
    below) for `AwsEventStreamParser`.
  - `lanius-cli/tests/gui_api_contract.rs` — contract tests protecting the
    interface between `lanius-core`'s `Config`/`server` and how
    `lanius-gui` embeds the gateway as a library rather than spawning
    `lanius-cli` as a subprocess. This exists specifically to catch
    breaking changes to the "GUI API surface" (which `Config` fields the
    desktop app maps its settings onto, and the `spawn`/`shutdown`
    lifecycle it drives) without needing to build the full Slint UI. It
    includes an end-to-end check: spawn the gateway on an OS-assigned
    ephemeral port, hit `/health`, shut it down gracefully, and confirm the
    listener actually closed.

## Property-based testing (`proptest`)

`parser_properties.rs` uses `proptest` to verify `AwsEventStreamParser` is
byte-boundary agnostic — a core correctness requirement, since Kiro's raw
byte stream can be chunked arbitrarily by the network stack and the parser
must produce identical results regardless of where the cuts land. It
checks:

- `parse_is_independent_of_chunk_boundaries` — feeding the same payload in
  one shot vs. arbitrary chunk sizes yields identical events.
- `byte_at_a_time_matches_whole` — feeding one byte at a time yields the
  same result as feeding the whole payload at once (the most adversarial
  chunking possible).
- `multibyte_content_survives_arbitrary_splits` — payloads containing
  multi-byte UTF-8 (CJK characters) survive arbitrary splits without
  corruption, specifically exercising the "don't split a UTF-8 sequence
  across chunks" logic in `decode_into_buffer`.
- `tool_arguments_are_split_independent` — a tool call's JSON arguments,
  streamed as several `input` fragments, reassemble identically regardless
  of how those fragments are further re-chunked at the byte level.

The same byte-boundary-independence property can be verified manually
against a real captured response using `lanius probe --capture` +
`lanius replay` at multiple `REPLAY_CHUNK_SIZE` values — see
[streaming.md](streaming.md#offline-debugging-lanius-probe--lanius-replay).

## Running a subset

```sh
# One crate
cargo test -p lanius-core

# One test (by substring match on the test name)
cargo test -p lanius-core is_content_truncated

# Doctests only
cargo test --workspace --doc

# Skip doctests (faster iteration on unit/integration tests)
cargo test --workspace --lib --tests
```

## Linting

```sh
cargo clippy --workspace --all-targets --all-features
```

`lanius-core`'s `lib.rs` sets `#![warn(clippy::all)]` and
`#![forbid(unsafe_code)]` crate-wide; there is no `unsafe` code anywhere in
`lanius-core`. `RUSTFLAGS="-W missing_docs" cargo build -p lanius-core --lib`
is a useful one-off check for doc-comment coverage on public items (every
public struct field, enum variant, and constant in `lanius-core` currently
has one).

## Writing new tests

Follow the existing convention: colocate a `#[cfg(test)] mod tests` at the
bottom of the file you're changing for anything that can be tested through
the module's own (possibly `pub(crate)`) API, and reach for
`crates/*/tests/` only when the thing under test is genuinely cross-module
or is specifically about the crate's *external* contract (as the GUI
contract test and tool-followup integration test are). Prefer a doctest
over a unit test when the thing you're testing usefully doubles as
documentation of how to call the function.
