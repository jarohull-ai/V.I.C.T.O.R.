//! Deterministyczny transport testowy. Nie otwiera żadnego gniazda.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use victor_integrations::IntegrationError;

use crate::transport::{HttpRequest, HttpResponse, HttpTransport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedResponse {
    Ok { status: u16, body: Vec<u8> },
    Timeout,
    TransportError,
}

impl ScriptedResponse {
    pub fn ok(body: Vec<u8>) -> Self {
        Self::Ok { status: 200, body }
    }

    pub fn status(status: u16) -> Self {
        Self::Ok {
            status,
            body: Vec::new(),
        }
    }

    pub fn status_with_body(status: u16, body: Vec<u8>) -> Self {
        Self::Ok { status, body }
    }
}

#[derive(Default)]
pub struct ScriptedTransport {
    queue: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            queue: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Inspekcja ostatniego requestu bez kopiowania audio.
    pub fn last_request<R>(&self, inspect: impl FnOnce(&HttpRequest) -> R) -> Option<R> {
        self.requests.lock().unwrap().last().map(inspect)
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, IntegrationError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| IntegrationError::Transport("brak scenariusza testowego".into()))?;
        match response {
            ScriptedResponse::Ok { status, body } => Ok(HttpResponse {
                status,
                body,
                content_type: Some("application/json".into()),
            }),
            ScriptedResponse::Timeout => Err(IntegrationError::Timeout(
                "scenariusz testowy: timeout".into(),
            )),
            ScriptedResponse::TransportError => Err(IntegrationError::Transport(
                "scenariusz testowy: błąd transportu".into(),
            )),
        }
    }
}
