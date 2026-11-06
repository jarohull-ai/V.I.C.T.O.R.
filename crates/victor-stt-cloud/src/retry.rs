//! Retry z twardymi limitami. Każda próba — także retry — najpierw sprawdza,
//! czy permit nadal pasuje do planu i nagrania.

use std::time::Duration;

use victor_domain::Plan;
use victor_integrations::{AudioChunk, CloudTranscriptionPermit, IntegrationError};

use crate::transport::{HttpRequest, HttpResponse, HttpTransport};

pub const MAX_ATTEMPTS: u32 = 5;
pub const MAX_BASE_DELAY: Duration = Duration::from_secs(10);
pub const MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Łączna liczba prób, włącznie z pierwszą. `1` = bez retry.
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.max_attempts == 0 || self.max_attempts > MAX_ATTEMPTS {
            return Err(IntegrationError::NotConfigured(format!(
                "liczba prób musi mieścić się w 1..={MAX_ATTEMPTS}"
            )));
        }
        if self.base_delay > MAX_BASE_DELAY {
            return Err(IntegrationError::NotConfigured(format!(
                "bazowe opóźnienie retry nie może przekraczać {}s",
                MAX_BASE_DELAY.as_secs()
            )));
        }
        Ok(())
    }

    fn delay_before(&self, next_attempt: u32) -> Duration {
        let exponent = next_attempt.saturating_sub(2).min(5);
        self.base_delay
            .saturating_mul(1u32 << exponent)
            .min(MAX_DELAY)
    }
}

/// Wykonuje żądanie z retry. `build` jest wołane od nowa przy każdej próbie,
/// a permit jest sprawdzany przed każdą z nich — zarówno hash planu, jak i
/// hash nagrania.
pub async fn execute_with_retry<T, F>(
    transport: &T,
    permit: &CloudTranscriptionPermit,
    plan: &Plan,
    audio: &AudioChunk,
    provider: &'static str,
    policy: RetryPolicy,
    build: F,
) -> Result<HttpResponse, IntegrationError>
where
    T: HttpTransport + ?Sized,
    F: Fn() -> Result<HttpRequest, IntegrationError>,
{
    policy.validate()?;

    let mut attempt = 1;
    loop {
        // Wymóg: weryfikacja przed każdym żądaniem i każdym retry.
        permit.verify(plan, audio)?;

        let request = build()?;
        let max_response_bytes = request.max_response_bytes();
        match transport.send(request).await {
            Ok(response) => {
                if response.body.len() as u64 > max_response_bytes {
                    return Err(IntegrationError::LimitExceeded(
                        "odpowiedź HTTP przekracza limit".into(),
                    ));
                }
                if !is_retryable_status(response.status) {
                    return Ok(response);
                }
                if attempt >= policy.max_attempts {
                    return Err(IntegrationError::provider(
                        provider,
                        format!("HTTP {} po {attempt} próbach", response.status),
                    ));
                }
            }
            Err(error) if is_retryable_error(&error) => {
                if attempt >= policy.max_attempts {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }

        tokio::time::sleep(policy.delay_before(attempt + 1)).await;
        attempt += 1;
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..=599).contains(&status)
}

fn is_retryable_error(error: &IntegrationError) -> bool {
    matches!(
        error,
        IntegrationError::Timeout(_) | IntegrationError::Transport(_) | IntegrationError::Io(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointAllowlist, ValidatedEndpoint};
    use crate::test_support;
    use crate::testing::{ScriptedResponse, ScriptedTransport};

    fn endpoint() -> ValidatedEndpoint {
        EndpointAllowlist::https("api.example", "/v1")
            .validate("https://api.example/v1/stt")
            .unwrap()
    }

    fn request() -> Result<HttpRequest, IntegrationError> {
        Ok(HttpRequest::post(endpoint(), Duration::from_secs(5), 1024))
    }

    async fn run(
        responses: Vec<ScriptedResponse>,
        policy: RetryPolicy,
        audio: victor_integrations::AudioChunk,
        permit_audio: &victor_integrations::AudioChunk,
    ) -> (Result<HttpResponse, IntegrationError>, usize) {
        let transport = ScriptedTransport::new(responses);
        let plan = test_support::plan();
        let permit = test_support::permit_for(&plan, permit_audio);
        let result =
            execute_with_retry(&transport, &permit, &plan, &audio, "test", policy, request).await;
        let calls = transport.request_count();
        (result, calls)
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let audio = test_support::wav();
        let (result, calls) = run(
            vec![
                ScriptedResponse::status(503),
                ScriptedResponse::status(503),
                ScriptedResponse::ok(b"{\"text\":\"x\"}".to_vec()),
            ],
            RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::ZERO,
            },
            audio.clone(),
            &audio,
        )
        .await;

        assert_eq!(result.unwrap().status, 200);
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn stops_after_max_attempts_without_leaking_body() {
        let audio = test_support::wav();
        let (result, calls) = run(
            vec![
                ScriptedResponse::status_with_body(500, b"SECRET-AUDIO-CONTENT".to_vec()),
                ScriptedResponse::status_with_body(500, b"SECRET-AUDIO-CONTENT".to_vec()),
            ],
            RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::ZERO,
            },
            audio.clone(),
            &audio,
        )
        .await;

        let error = result.unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("500"));
        assert!(!rendered.contains("SECRET-AUDIO-CONTENT"));
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        let audio = test_support::wav();
        let (result, calls) = run(
            vec![ScriptedResponse::status_with_body(
                400,
                b"bad request".to_vec(),
            )],
            RetryPolicy {
                max_attempts: 5,
                base_delay: Duration::ZERO,
            },
            audio.clone(),
            &audio,
        )
        .await;

        assert_eq!(result.unwrap().status, 400);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retries_timeout() {
        let audio = test_support::wav();
        let (result, calls) = run(
            vec![
                ScriptedResponse::Timeout,
                ScriptedResponse::ok(b"{}".to_vec()),
            ],
            RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::ZERO,
            },
            audio.clone(),
            &audio,
        )
        .await;

        assert_eq!(result.unwrap().status, 200);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn verifies_audio_before_any_request() {
        let permitted_audio = test_support::wav();
        let mut other_audio = permitted_audio.clone();
        other_audio.bytes.push(99);

        let (result, calls) = run(
            vec![ScriptedResponse::ok(b"{}".to_vec())],
            RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::ZERO,
            },
            other_audio,
            &permitted_audio,
        )
        .await;

        assert!(matches!(result, Err(IntegrationError::Denied(_))));
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn verifies_plan_before_any_request() {
        let audio = test_support::wav();
        let plan = test_support::plan();
        let permit = test_support::permit_for(&plan, &audio);
        let mut other_plan = plan.clone();
        other_plan.intent = "inny cel".into();

        let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(b"{}".to_vec())]);
        let result = execute_with_retry(
            &transport,
            &permit,
            &other_plan,
            &audio,
            "test",
            RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::ZERO,
            },
            request,
        )
        .await;

        assert!(matches!(result, Err(IntegrationError::Denied(_))));
        assert_eq!(transport.request_count(), 0);
    }

    #[tokio::test]
    async fn rejects_response_over_limit() {
        let audio = test_support::wav();
        let (result, calls) = run(
            vec![ScriptedResponse::ok(vec![0; 2048])],
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
            },
            audio.clone(),
            &audio,
        )
        .await;

        assert!(matches!(result, Err(IntegrationError::LimitExceeded(_))));
        assert_eq!(calls, 1);
    }

    #[test]
    fn delay_grows_but_is_capped() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
        };
        assert_eq!(policy.delay_before(2), Duration::from_secs(1));
        assert_eq!(policy.delay_before(3), Duration::from_secs(2));
        assert_eq!(policy.delay_before(4), Duration::from_secs(4));
        assert_eq!(policy.delay_before(10), MAX_DELAY);
    }

    #[test]
    fn rejects_invalid_policy() {
        let policy = RetryPolicy {
            max_attempts: 0,
            ..RetryPolicy::default()
        };
        assert!(policy.validate().is_err());

        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_secs(60),
        };
        assert!(policy.validate().is_err());
    }
}
