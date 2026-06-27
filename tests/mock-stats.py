#!/usr/bin/env python3
"""Tiny mock of the miner's /1/summary endpoint for dashboard dry-runs.

Usage: python3 tests/mock-stats.py [fixture.json] [port]
Serves the given fixture (default tests/fixtures/summary_full.json) at
GET /1/summary, and {"ok":true} at /healthz. Loopback only. No deps.
"""
import sys, os
from http.server import BaseHTTPRequestHandler, HTTPServer

FIX = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "fixtures", "summary_full.json")
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 13380

with open(FIX, "rb") as f:
    BODY = f.read()


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/1/summary"):
            body = BODY
        elif self.path.startswith("/healthz"):
            body = b'{"ok":true}'
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    print(f"mock /1/summary -> {FIX} on http://127.0.0.1:{PORT}", flush=True)
    HTTPServer(("127.0.0.1", PORT), H).serve_forever()
