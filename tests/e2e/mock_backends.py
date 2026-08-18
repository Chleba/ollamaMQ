import json, time, threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse

import os
CALLS = os.environ.get("MQ_E2E_CALLS", "/tmp/mq_e2e_calls.jsonl")

def log(tag, path, body):
    with open(CALLS, "a") as f:
        f.write(json.dumps({"server": tag, "path": path, "body": body, "ts": time.time()}) + "\n")

class OllamaMock(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        p = urlparse(self.path).path
        if p == "/api/tags":
            self._send(200, {"models": [{"name": "llama3:latest"}, {"name": "qwen2.5:7b"}]})
        elif p == "/api/ps":
            self._send(200, {"models": [{"name": "qwen2.5:7b", "size_vram": 1}]})
        else:
            self._send(404, {"error": "not found"})
    def do_POST(self):
        p = urlparse(self.path).path
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        if p == "/api/generate":
            log("ollama", p, body)
            if body.get("keep_alive") == 0:
                self._send(200, {"model": body.get("model"), "done": True, "done_reason": "unload"})
            else:
                self._send(200, {"model": body.get("model"), "done": True, "response": ""})
        elif p == "/api/chat":
            # hang for 12s to keep the backend "busy" (E2E busy test)
            time.sleep(12)
            self._send(200, {"model": body.get("model"), "message": {"role": "assistant", "content": "hi"}, "done": True})
        else:
            self._send(404, {"error": "not found"})

class LMMock(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        p = urlparse(self.path).path
        if p == "/v1/models":
            self._send(200, {"data": [{"id": "mock/qwen2-7b-instruct"}, {"id": "mock/llama-8b"}]})
        elif p == "/api/v1/models":
            self._send(200, {"models": [
                {"key": "mock/qwen2-7b-instruct", "id": "mock/qwen2-7b-instruct",
                 "display_name": "Mock Qwen2 7B Instruct",
                 "loaded_instances": [{"id": "mock/qwen2-7b-instruct"}]},
                {"key": "mock/llama-8b", "id": "mock/llama-8b",
                 "display_name": "Mock Llama 8B", "loaded_instances": []},
            ]})
        elif p == "/":
            self._send(200, {"ok": True})
        else:
            self._send(404, {"error": "not found"})
    def do_POST(self):
        p = urlparse(self.path).path
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        if p == "/api/v1/models/load":
            log("lmstudio", p, body)
            time.sleep(2)  # slow load -> window for duplicate-op 409 test
            self._send(200, {"type": "llm", "instance_id": body.get("model"),
                             "status": "loaded", "load_time_seconds": 2.0})
        elif p == "/api/v1/models/unload":
            log("lmstudio", p, body)
            self._send(200, {"instance_id": body.get("instance_id")})
        else:
            self._send(404, {"error": "not found"})

open(CALLS, "w").close()
servers = [
    HTTPServer(("127.0.0.1", 11990), OllamaMock),
    HTTPServer(("127.0.0.1", 11991), LMMock),
]
for s in servers:
    threading.Thread(target=s.serve_forever, daemon=True).start()
print("mocks up")
time.sleep(3600)
