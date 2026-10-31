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
    /// Title generation (recognized by its system prompt) has its own queue and log, so it never disturbs
    /// tests that count or order the main conversation's requests. An empty queue answers with nothing.
    titles: Mutex<std::collections::VecDeque<Scripted>>,
    pub title_requests: Mutex<Vec<Request>>,
}

impl FakeProvider {
    pub fn new(script: Vec<Scripted>) -> Self {
        FakeProvider { script: Mutex::new(script.into()), requests: Mutex::new(vec![]), titles: Mutex::new(Default::default()), title_requests: Mutex::new(vec![]) }
    }
    /// Queue more responses (for multi-turn tests where later script entries depend on earlier results).
    pub fn extend(&self, more: Vec<Scripted>) {
        self.script.lock().unwrap().extend(more);
    }
    /// Queue replies for title requests.
    pub fn script_titles(&self, more: Vec<Scripted>) {
        self.titles.lock().unwrap().extend(more);
    }
    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Provider for FakeProvider {
    fn stream(&self, req: &Request, cancel: &AtomicBool, on_event: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
        let is_title = req.system == crate::title::SYSTEM;
        if is_title {
            self.title_requests.lock().unwrap().push(req.clone());
        } else {
            self.requests.lock().unwrap().push(req.clone());
        }
        let next = if is_title {
            match self.titles.lock().unwrap().pop_front() {
                Some(n) => n,
                None => return Ok(()),
            }
        } else {
            self.script.lock().unwrap().pop_front().expect("FakeProvider script exhausted")
        };
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
