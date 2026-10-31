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
    pub routed_requests: Mutex<Vec<Request>>,
    /// Requests whose first user message contains a marker get their own script (for work on other threads, such as
    /// background subagents, whose request order against the main conversation is otherwise a race).
    routes: Mutex<Vec<(String, std::collections::VecDeque<Scripted>)>>,
}

impl FakeProvider {
    pub fn new(script: Vec<Scripted>) -> Self {
        FakeProvider { script: Mutex::new(script.into()), requests: Mutex::new(vec![]), titles: Mutex::new(Default::default()), title_requests: Mutex::new(vec![]), routes: Mutex::new(vec![]), routed_requests: Mutex::new(vec![]) }
    }
    /// Queue more responses (for multi-turn tests where later script entries depend on earlier results).
    pub fn extend(&self, more: Vec<Scripted>) {
        self.script.lock().unwrap().extend(more);
    }
    /// Answer requests whose first user message contains `marker` from `script` instead of the main script.
    pub fn script_route(&self, marker: &str, script: Vec<Scripted>) {
        self.routes.lock().unwrap().push((marker.to_string(), script.into()));
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
        let first_user = req
            .messages
            .first()
            .map(|m| m.content.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        // Decide where the request belongs before recording it, so it is recorded exactly once, in one place
        // (other threads are recording theirs at the same time).
        let route = if is_title { None } else { self.routes.lock().unwrap().iter().position(|(m, _)| first_user.contains(m.as_str())) };
        let next = match route {
            Some(i) => {
                self.routed_requests.lock().unwrap().push(req.clone());
                match self.routes.lock().unwrap()[i].1.pop_front() {
                    Some(r) => r,
                    // Falling through to the main script would silently eat the main conversation's replies.
                    None => return Err(ProviderError::InvalidRequest("the scripted route for this request is exhausted".into())),
                }
            }
            None if is_title => {
                self.title_requests.lock().unwrap().push(req.clone());
                match self.titles.lock().unwrap().pop_front() {
                    Some(n) => n,
                    None => return Ok(()),
                }
            }
            None => {
                self.requests.lock().unwrap().push(req.clone());
                self.script.lock().unwrap().pop_front().expect("FakeProvider script exhausted")
            }
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
