//! Scripted provider for tests: replays a queue of canned responses, one per `stream` call.
use super::*;
use std::sync::Mutex;

pub enum Scripted {
    /// Events delivered in order, then Ok.
    Events(Vec<Event>),
    /// Events delivered, then this error (a failure part-way through a stream).
    Fail(Vec<Event>, ProviderError),
}

pub struct FakeProvider {
    script: Mutex<std::collections::VecDeque<Scripted>>,
    pub requests: Mutex<Vec<Request>>,
}

impl FakeProvider {
    pub fn new(script: Vec<Scripted>) -> Self {
        FakeProvider { script: Mutex::new(script.into()), requests: Mutex::new(vec![]) }
    }
    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Provider for FakeProvider {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let next = self.script.lock().unwrap().pop_front().expect("FakeProvider script exhausted");
        let (events, err) = match next {
            Scripted::Events(e) => (e, None),
            Scripted::Fail(e, err) => (e, Some(err)),
        };
        for ev in events {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(ProviderError::Interrupted);
            }
            on_event(ev);
        }
        err.map_or(Ok(()), Err)
    }
}

pub fn text(t: &str) -> Scripted {
    Scripted::Events(vec![Event::TextDelta(t.into()), Event::Finish { reason: FinishReason::EndTurn, usage: Usage { input: 10, output: 5, ..Default::default() } }])
}

pub fn tool_call(id: &str, name: &str, input: Value) -> Scripted {
    Scripted::Events(vec![
        Event::ToolInputStart { id: id.into(), name: name.into() },
        Event::ToolCall { id: id.into(), name: name.into(), input },
        Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 10, output: 5, ..Default::default() } },
    ])
}
