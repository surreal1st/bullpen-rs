//! A stand-in for the model boundary that replays a fixed sequence of
//! Bullpen's OWN event shape. Port of `test/helpers/fake-port.ts`. It does
//! not fake the OpenRouter wire format: that is `parse_sse_stream`, tested
//! separately against captured real responses so nothing here invents what
//! the provider actually sends.

use std::sync::{Arc, Mutex};

use futures::stream;

use crate::port::{EventStream, ModelEvent, ModelPort, ModelRequest, ModelUsage};

#[derive(Clone)]
pub struct FakePort {
    events: Vec<ModelEvent>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl FakePort {
    pub fn new(events: Vec<ModelEvent>) -> Self {
        Self {
            events,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request the app sent across the model boundary, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("fake port request log poisoned")
            .clone()
    }
}

impl ModelPort for FakePort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests
            .lock()
            .expect("fake port request log poisoned")
            .push(request);
        Box::pin(stream::iter(self.events.clone()))
    }
}

fn chunk(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

/// A port that also reports provider usage, the way OpenRouter does.
pub fn billed_port(text: &str, cost_usd: f64, model: &str) -> FakePort {
    let mut events: Vec<ModelEvent> = chunk(text, 7)
        .into_iter()
        .map(|t| ModelEvent::Delta { text: t })
        .collect();
    events.push(ModelEvent::Done {
        model: model.to_string(),
        usage: Some(ModelUsage {
            cost_usd,
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 64,
        }),
        finish_reason: None,
    });
    FakePort::new(events)
}

pub fn text_port(text: &str, model: &str) -> FakePort {
    let mut events: Vec<ModelEvent> = chunk(text, 7)
        .into_iter()
        .map(|t| ModelEvent::Delta { text: t })
        .collect();
    events.push(ModelEvent::Done {
        model: model.to_string(),
        usage: None,
        finish_reason: None,
    });
    FakePort::new(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn text_port_replays_deltas_then_done_and_records_the_request() {
        let port = text_port("hello", "test/model");
        let req = ModelRequest {
            model: "requested/model".into(),
            ..Default::default()
        };
        let events: Vec<ModelEvent> = port.stream(req).collect().await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello");
        assert!(
            matches!(events.last(), Some(ModelEvent::Done { model, .. }) if model == "test/model")
        );
        assert_eq!(port.requests().len(), 1);
        assert_eq!(port.requests()[0].model, "requested/model");
    }
}
