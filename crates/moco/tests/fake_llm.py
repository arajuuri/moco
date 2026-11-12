#!/usr/bin/env python3
"""A local stand-in for the OpenAI Responses and Gemini endpoints, for moco's end-to-end protocol test.

Usage: fake_llm.py VIOLATIONS_FILE   (prints the port on the first line of stdout)

Every conversation takes two turns: the first answers with a tool call (preceded by the provider-private replay data a
real reasoning model sends), the second must contain that replay data and the tool result, and answers with text that
quotes the file. Anything wrong with the second request is appended to VIOLATIONS_FILE, which the test reads."""
import json, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer

VIOLATIONS = sys.argv[1]


def violation(msg):
    with open(VIOLATIONS, "a") as f:
        f.write(msg + "\n")


def sse(handler, chunks, event_lines=False):
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.send_header("Connection", "close")
    handler.end_headers()
    for c in chunks:
        if event_lines:
            handler.wfile.write(("event: %s\n" % c["type"]).encode())
        handler.wfile.write(("data: %s\n\n" % json.dumps(c)).encode())
    handler.wfile.flush()


def responses(handler, body):
    if "next message a user might send" in body.get("instructions", ""):  # the TUI's suggested next message
        return sse(handler, [
            {"type": "response.output_text.delta", "delta": "Run the tests"},
            {"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 40, "output_tokens": 4}}}], True)
    if "short titles" in body.get("instructions", ""):
        return sse(handler, [
            {"type": "response.output_text.delta", "delta": "Fake protocol test"},
            {"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 5, "output_tokens": 3}}}], True)
    items = body["input"]
    outputs = [i for i in items if i.get("type") == "function_call_output"]
    if "BACKGROUND-JOB" in json.dumps(items):  # #150: a background job that finishes after the turn has ended
        return background_job(handler, items, outputs)
    if not outputs:
        if "SLOW" in json.dumps(items):
            time.sleep(1.5)  # lets a test type while the turn is running
        reasoning = {"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC-1", "summary": []}
        call = {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": json.dumps({"path": "a.txt"}), "status": "completed"}
        return sse(handler, [
            {"type": "response.output_item.done", "output_index": 0, "item": reasoning},
            {"type": "response.output_text.delta", "delta": "Reading the file."},
            {"type": "response.output_item.added", "output_index": 1, "item": dict(call, arguments="")},
            {"type": "response.output_item.done", "output_index": 1, "item": call},
            {"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 20}}}], True)
    # Second request: the replay must be exact.
    if not any(i.get("type") == "reasoning" and i.get("encrypted_content") == "ENC-1" for i in items):
        violation("responses: the reasoning item was not replayed")
    if not any(i.get("type") == "function_call" and i.get("id") == "fc_1" and i.get("call_id") == "call_1" for i in items):
        violation("responses: the original function_call item was not replayed")
    if body.get("store") is not False:
        violation("responses: store must be false")
    if "reasoning.encrypted_content" not in body.get("include", []):
        violation("responses: encrypted reasoning was not requested")
    out = outputs[0]["output"]
    text = "The file says: " + out.split(": ", 1)[-1].strip()
    sse(handler, [
        {"type": "response.output_text.delta", "delta": text},
        {"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 150, "input_tokens_details": {"cached_tokens": 100}, "output_tokens": 12}}}], True)


def background_job(handler, items, outputs):
    done = {"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 50, "output_tokens": 5}}}
    if "JOB-RESULT-42" in json.dumps(items):  # the job's result was delivered: report it
        return sse(handler, [{"type": "response.output_text.delta", "delta": "The job printed JOB-RESULT-42."}, done], True)
    if outputs:  # the job was started: end the turn, as a model waiting to be told does
        return sse(handler, [{"type": "response.output_text.delta", "delta": "Started it; waiting for the result."}, done], True)
    call = {"type": "function_call", "id": "fc_bg", "call_id": "call_bg", "name": "shell", "status": "completed",
            "arguments": json.dumps({"command": "sleep 1; echo JOB-RESULT-$((40 + 2))", "background": True})}
    return sse(handler, [
        {"type": "response.output_item.added", "output_index": 0, "item": dict(call, arguments="")},
        {"type": "response.output_item.done", "output_index": 0, "item": call}, done], True)


def gemini(handler, body):
    if "short titles" in json.dumps(body.get("systemInstruction", {})):
        return sse(handler, [{"candidates": [{"content": {"parts": [{"text": "Fake protocol test"}], "role": "model"}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3}}])
    contents = body["contents"]
    responses_ = [p["functionResponse"] for c in contents for p in c["parts"] if "functionResponse" in p]
    if not responses_:
        return sse(handler, [
            {"candidates": [{"content": {"parts": [{"text": "Reading the file."}], "role": "model"}}], "usageMetadata": {"promptTokenCount": 100}},
            {"candidates": [{"content": {"parts": [{"functionCall": {"name": "read", "args": {"path": "a.txt"}}, "thoughtSignature": "SIG-1"}], "role": "model"}, "finishReason": "STOP"}],
             "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 8, "thoughtsTokenCount": 12}}])
    calls = [p for c in contents if c["role"] == "model" for p in c["parts"] if "functionCall" in p]
    if not calls or calls[0].get("thoughtSignature") != "SIG-1":
        violation("gemini: the thought signature was not sent back on the function call")
    if responses_[0]["name"] != "read":
        violation("gemini: the function response is not matched to the function by name")
    params = json.dumps(body.get("tools", []))
    if "additionalProperties" in params or '"$schema"' in params:
        violation("gemini: unsupported schema keywords were sent")
    out = responses_[0]["response"].get("output", "")
    sse(handler, [{"candidates": [{"content": {"parts": [{"text": "The file says: " + out.split(": ", 1)[-1].strip()}], "role": "model"}, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 150, "candidatesTokenCount": 12, "cachedContentTokenCount": 100}}])


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path.startswith("/v1/responses"):
            responses(self, body)
        elif self.path.startswith("/v1beta/models/m:streamGenerateContent"):
            if not self.headers.get("x-goog-api-key") and self.headers.get("x-test-needs-key"):
                violation("gemini: no api key header")
            gemini(self, body)
        else:
            violation("unexpected path " + self.path)
            self.send_response(404)
            self.end_headers()

    def log_message(self, *a):
        pass


s = HTTPServer(("127.0.0.1", 0), H)
print(s.server_address[1], flush=True)
s.serve_forever()
