//! Generates ready-to-copy API usage snippets (curl / Python / TypeScript)
//! for the "API examples" panel in the GUI, showing users how to call the
//! locally running gateway with either the OpenAI-compatible or
//! Anthropic-compatible API surface.
//!
//! `controller.rs` drives this module: it tracks which [`ApiFlavor`] and
//! [`Snippet`] tab the user has selected and calls [`render`] to regenerate
//! the displayed code whenever the selection, host, port, or API key
//! changes. Two renders are typically produced per update — one with the
//! real API key (for the "copy" action) and one with the key masked via
//! [`mask_key`] (for on-screen display), so the real secret is never shown
//! but can still be copied correctly.

/// Which API surface (wire format) a generated example targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFlavor {
    OpenAi,
    Anthropic,
}

/// Which client language/tool a generated example is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snippet {
    Curl,
    Python,
    TypeScript,
}

impl ApiFlavor {
    /// Maps a Slint UI tab index to an [`ApiFlavor`]. Any index other than
    /// `1` (including out-of-range values) falls back to [`ApiFlavor::OpenAi`],
    /// so an unexpected index from the UI never panics.
    pub fn from_index(index: i32) -> Self {
        if index == 1 {
            ApiFlavor::Anthropic
        } else {
            ApiFlavor::OpenAi
        }
    }
}

impl Snippet {
    /// Maps a Slint UI tab index to a [`Snippet`] language. Any index other
    /// than `1` or `2` (including out-of-range values) falls back to
    /// [`Snippet::Curl`].
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Snippet::Python,
            2 => Snippet::TypeScript,
            _ => Snippet::Curl,
        }
    }
}

const MODEL: &str = "claude-opus-5";

/// Masks the secret portion of an `sk-...`-style API key for on-screen
/// display, replacing every character after the `sk-` prefix with a bullet
/// (`•`) so the real key is never visible in the rendered example while its
/// length still hints that a key is present. Keys that don't start with
/// `sk-` (or placeholder text) are returned unchanged.
pub fn mask_key(api_key: &str) -> String {
    match api_key.strip_prefix("sk-") {
        Some(rest) if !rest.is_empty() => format!("sk-{}", "•".repeat(rest.chars().count())),
        _ => api_key.to_string(),
    }
}

/// Renders a complete, copy-pasteable code snippet demonstrating how to call
/// the local gateway with the given `flavor`/`snippet` combination.
///
/// `host`/`port` describe where the gateway is listening; a wildcard bind
/// address (`0.0.0.0`, meaning "listen on all interfaces") is rewritten to
/// `127.0.0.1` because `0.0.0.0` is not itself a valid address a client can
/// connect *to*. If `api_key` is empty (e.g. no key configured yet), the
/// placeholder `YOUR_API_KEY` is substituted so the snippet remains valid,
/// syntactically complete example code.
pub fn render(flavor: ApiFlavor, snippet: Snippet, host: &str, port: u16, api_key: &str) -> String {
    let host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    let base_url = format!("http://{host}:{port}");
    let key = if api_key.is_empty() {
        "YOUR_API_KEY"
    } else {
        api_key
    };

    match (flavor, snippet) {
        (ApiFlavor::OpenAi, Snippet::Curl) => format!(
            r#"curl {base_url}/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer {key}" \
  -d '{{
    "model": "{MODEL}",
    "messages": [
      {{"role": "user", "content": "Hello!"}}
    ]
  }}'"#
        ),
        (ApiFlavor::OpenAi, Snippet::Python) => format!(
            r#"from openai import OpenAI

client = OpenAI(
    base_url="{base_url}/v1",
    api_key="{key}"
)

response = client.chat.completions.create(
    model="{MODEL}",
    messages=[
        {{"role": "user", "content": "Hello!"}}
    ]
)

print(response.choices[0].message.content)"#
        ),
        (ApiFlavor::OpenAi, Snippet::TypeScript) => format!(
            r#"import OpenAI from 'openai';

const client = new OpenAI({{
  baseURL: '{base_url}/v1',
  apiKey: '{key}',
}});

const response = await client.chat.completions.create({{
  model: '{MODEL}',
  messages: [
    {{ role: 'user', content: 'Hello!' }}
  ],
}});

console.log(response.choices[0].message.content);"#
        ),
        (ApiFlavor::Anthropic, Snippet::Curl) => format!(
            r#"curl {base_url}/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: {key}" \
  -H "anthropic-version: 2023-06-01" \
  -d '{{
    "model": "{MODEL}",
    "max_tokens": 1024,
    "messages": [
      {{"role": "user", "content": "Hello!"}}
    ]
  }}'"#
        ),
        (ApiFlavor::Anthropic, Snippet::Python) => format!(
            r#"import anthropic

client = anthropic.Anthropic(
    base_url="{base_url}/v1",
    api_key="{key}"
)

message = client.messages.create(
    model="{MODEL}",
    max_tokens=1024,
    messages=[
        {{"role": "user", "content": "Hello!"}}
    ]
)

print(message.content[0].text)"#
        ),
        (ApiFlavor::Anthropic, Snippet::TypeScript) => format!(
            r#"import Anthropic from '@anthropic-ai/sdk';

const client = new Anthropic({{
  baseURL: '{base_url}/v1',
  apiKey: '{key}',
}});

const message = await client.messages.create({{
  model: '{MODEL}',
  max_tokens: 1024,
  messages: [
    {{ role: 'user', content: 'Hello!' }}
  ],
}});

console.log(message.content[0].text);"#
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_curl_targets_the_chat_completions_endpoint() {
        let code = render(
            ApiFlavor::OpenAi,
            Snippet::Curl,
            "127.0.0.1",
            8000,
            "sk-abc",
        );
        assert!(code.contains("http://127.0.0.1:8000/v1/chat/completions"));
        assert!(code.contains("Authorization: Bearer sk-abc"));
        assert!(code.contains("claude-opus-5"));
    }

    #[test]
    fn mask_key_hides_only_the_secret_part_of_an_sk_key() {
        assert_eq!(mask_key("sk-abc123"), "sk-••••••");
        assert_eq!(mask_key("YOUR_API_KEY"), "YOUR_API_KEY");
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("sk-"), "sk-");
    }

    #[test]
    fn masked_key_never_leaks_into_the_rendered_snippet() {
        let key = "sk-86a7IRCLLmH9KpO9bLP0Ww75eyXJWH2T";
        let masked = mask_key(key);
        let display = render(ApiFlavor::OpenAi, Snippet::Curl, "127.0.0.1", 8000, &masked);
        assert!(!display.contains(key));
        assert!(display.contains("sk-••"));
    }

    #[test]
    fn anthropic_snippets_use_the_messages_endpoint_and_header() {
        let code = render(ApiFlavor::Anthropic, Snippet::Curl, "127.0.0.1", 9000, "k");
        assert!(code.contains("/v1/messages"));
        assert!(code.contains("x-api-key: k"));
        assert!(code.contains("anthropic-version: 2023-06-01"));
        assert!(!code.contains("Authorization:"));
    }

    #[test]
    fn wildcard_host_is_rendered_as_loopback() {
        for flavor in [ApiFlavor::OpenAi, ApiFlavor::Anthropic] {
            for snippet in [Snippet::Curl, Snippet::Python, Snippet::TypeScript] {
                let code = render(flavor, snippet, "0.0.0.0", 8000, "k");
                assert!(
                    !code.contains("0.0.0.0"),
                    "0.0.0.0 is not connectable: {code}"
                );
                assert!(code.contains("127.0.0.1:8000"));
            }
        }
    }

    #[test]
    fn missing_key_falls_back_to_a_placeholder() {
        let code = render(ApiFlavor::OpenAi, Snippet::Python, "127.0.0.1", 8000, "");
        assert!(code.contains("YOUR_API_KEY"));
    }

    #[test]
    fn indices_map_to_the_ui_tab_order() {
        assert_eq!(ApiFlavor::from_index(0), ApiFlavor::OpenAi);
        assert_eq!(ApiFlavor::from_index(1), ApiFlavor::Anthropic);
        assert_eq!(
            ApiFlavor::from_index(7),
            ApiFlavor::OpenAi,
            "out of range is safe"
        );
        assert_eq!(Snippet::from_index(0), Snippet::Curl);
        assert_eq!(Snippet::from_index(1), Snippet::Python);
        assert_eq!(Snippet::from_index(2), Snippet::TypeScript);
        assert_eq!(Snippet::from_index(-1), Snippet::Curl);
    }

    #[test]
    fn braces_are_emitted_literally() {
        let code = render(
            ApiFlavor::OpenAi,
            Snippet::TypeScript,
            "127.0.0.1",
            8000,
            "k",
        );
        assert!(code.contains("const client = new OpenAI({"));
        assert!(code.contains("});"));
        assert!(!code.contains("{{"), "escaped braces must not leak: {code}");
    }
}
