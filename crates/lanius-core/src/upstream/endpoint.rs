//! Chat endpoint rotation for `generateAssistantResponse`.
//!
//! Kiro exposes the same chat operation behind several host / `x-amz-target`
//! combinations that are rate-limited independently. When one of them answers
//! `429`, [`EndpointThrottle`] parks it for [`THROTTLE_DURATION`] and
//! [`KiroHttpClient::chat_request_with_retry`](super::KiroHttpClient::chat_request_with_retry)
//! moves on to the next one instead of backing off on the same endpoint.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

/// How long an endpoint is skipped after it returned `429`.
pub const THROTTLE_DURATION: Duration = Duration::from_secs(30);

const CODEWHISPERER_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
const AMAZON_Q_TARGET: &str = "AmazonQDeveloperStreamingService.SendMessage";

/// Identifies one chat endpoint variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatEndpointKind {
    /// `runtime.<region>.kiro.dev`; requires a profile ARN.
    Runtime,
    /// `q.<region>.amazonaws.com` with the CodeWhisperer target.
    Q,
    /// `codewhisperer.us-east-1.amazonaws.com` (only exists in us-east-1).
    CodeWhisperer,
    /// `q.<region>.amazonaws.com` with the Amazon Q `SendMessage` target.
    AmazonQ,
}

impl ChatEndpointKind {
    /// Short name used in logs.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert_eq!(ChatEndpointKind::Runtime.as_str(), "runtime");
    /// assert_eq!(ChatEndpointKind::AmazonQ.as_str(), "amazonq");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Q => "q",
            Self::CodeWhisperer => "codewhisperer",
            Self::AmazonQ => "amazonq",
        }
    }
}

/// A concrete chat endpoint: full URL plus the `x-amz-target` it expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEndpoint {
    /// Which variant this is.
    pub kind: ChatEndpointKind,
    /// Full `.../generateAssistantResponse` URL.
    pub url: String,
    /// Value for the `x-amz-target` header.
    pub amz_target: &'static str,
}

/// Returns the chat endpoints for `region` in preference order. The first
/// entry matches the host Lanius used before rotation existed (runtime when a
/// profile ARN is known, `q` otherwise).
///
/// # Examples
///
/// ```ignore
/// let endpoints = chat_endpoints("us-east-1", true);
/// assert_eq!(endpoints[0].kind, ChatEndpointKind::Runtime);
/// assert_eq!(endpoints.len(), 4);
/// ```
pub fn chat_endpoints(region: &str, has_profile: bool) -> Vec<ChatEndpoint> {
    let region = region.to_ascii_lowercase();
    let endpoint = |kind, host: String, amz_target| ChatEndpoint {
        kind,
        url: format!("https://{host}/generateAssistantResponse"),
        amz_target,
    };
    let mut endpoints = Vec::with_capacity(4);
    if has_profile {
        endpoints.push(endpoint(
            ChatEndpointKind::Runtime,
            format!("runtime.{region}.kiro.dev"),
            CODEWHISPERER_TARGET,
        ));
    }
    endpoints.push(endpoint(
        ChatEndpointKind::Q,
        format!("q.{region}.amazonaws.com"),
        CODEWHISPERER_TARGET,
    ));
    if region == "us-east-1" {
        endpoints.push(endpoint(
            ChatEndpointKind::CodeWhisperer,
            "codewhisperer.us-east-1.amazonaws.com".to_string(),
            CODEWHISPERER_TARGET,
        ));
    }
    endpoints.push(endpoint(
        ChatEndpointKind::AmazonQ,
        format!("q.{region}.amazonaws.com"),
        AMAZON_Q_TARGET,
    ));
    endpoints
}

/// Tracks which endpoints are currently parked after a `429`.
#[derive(Debug, Default)]
pub struct EndpointThrottle {
    until: Mutex<HashMap<ChatEndpointKind, Instant>>,
}

/// Process-wide throttle state shared by every request.
pub static GLOBAL_THROTTLE: Lazy<EndpointThrottle> = Lazy::new(EndpointThrottle::default);

impl EndpointThrottle {
    /// Parks `kind` for `duration`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let throttle = EndpointThrottle::default();
    /// throttle.throttle(ChatEndpointKind::Q, THROTTLE_DURATION);
    /// assert!(throttle.is_throttled(ChatEndpointKind::Q));
    /// ```
    pub fn throttle(&self, kind: ChatEndpointKind, duration: Duration) {
        self.lock().insert(kind, Instant::now() + duration);
    }

    /// Whether `kind` is still parked.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let throttle = EndpointThrottle::default();
    /// assert!(!throttle.is_throttled(ChatEndpointKind::Runtime));
    /// throttle.throttle(ChatEndpointKind::Runtime, Duration::from_secs(30));
    /// assert!(throttle.is_throttled(ChatEndpointKind::Runtime));
    /// ```
    pub fn is_throttled(&self, kind: ChatEndpointKind) -> bool {
        let mut until = self.lock();
        match until.get(&kind) {
            Some(deadline) if Instant::now() < *deadline => true,
            Some(_) => {
                until.remove(&kind);
                false
            }
            None => false,
        }
    }

    /// Returns the index of the first endpoint that is not parked, or `None`
    /// when every endpoint is parked.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let throttle = EndpointThrottle::default();
    /// let endpoints = chat_endpoints("us-east-1", true);
    /// throttle.throttle(ChatEndpointKind::Runtime, THROTTLE_DURATION);
    /// assert_eq!(throttle.pick(&endpoints), Some(1));
    /// ```
    pub fn pick(&self, endpoints: &[ChatEndpoint]) -> Option<usize> {
        endpoints
            .iter()
            .position(|endpoint| !self.is_throttled(endpoint.kind))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ChatEndpointKind, Instant>> {
        self.until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(endpoints: &[ChatEndpoint]) -> Vec<ChatEndpointKind> {
        endpoints.iter().map(|endpoint| endpoint.kind).collect()
    }

    #[test]
    fn us_east_1_with_profile_has_all_four_runtime_first() {
        let endpoints = chat_endpoints("us-east-1", true);
        assert_eq!(
            kinds(&endpoints),
            vec![
                ChatEndpointKind::Runtime,
                ChatEndpointKind::Q,
                ChatEndpointKind::CodeWhisperer,
                ChatEndpointKind::AmazonQ,
            ]
        );
        assert_eq!(
            endpoints[0].url,
            "https://runtime.us-east-1.kiro.dev/generateAssistantResponse"
        );
        assert_eq!(endpoints[3].amz_target, AMAZON_Q_TARGET);
    }

    #[test]
    fn profileless_skips_runtime_and_other_regions_skip_codewhisperer() {
        let endpoints = chat_endpoints("EU-Central-1", false);
        assert_eq!(
            kinds(&endpoints),
            vec![ChatEndpointKind::Q, ChatEndpointKind::AmazonQ]
        );
        assert_eq!(
            endpoints[0].url,
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
    }

    #[test]
    fn pick_skips_throttled_endpoints_and_recovers_after_expiry() {
        let throttle = EndpointThrottle::default();
        let endpoints = chat_endpoints("us-east-1", true);
        assert_eq!(throttle.pick(&endpoints), Some(0));

        throttle.throttle(ChatEndpointKind::Runtime, THROTTLE_DURATION);
        assert_eq!(throttle.pick(&endpoints), Some(1));

        throttle.throttle(ChatEndpointKind::Q, Duration::from_millis(10));
        assert_eq!(throttle.pick(&endpoints), Some(2));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(throttle.pick(&endpoints), Some(1));
    }

    #[test]
    fn pick_returns_none_when_everything_is_throttled() {
        let throttle = EndpointThrottle::default();
        let endpoints = chat_endpoints("eu-west-1", false);
        for endpoint in &endpoints {
            throttle.throttle(endpoint.kind, THROTTLE_DURATION);
        }
        assert_eq!(throttle.pick(&endpoints), None);
    }
}
