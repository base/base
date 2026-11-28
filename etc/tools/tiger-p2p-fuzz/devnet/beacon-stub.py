"""Minimal beacon API for base-consensus against anvil: genesis time and slot length only.
No blobs are ever needed (no batcher runs), so blob sidecar requests answer 404."""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

GENESIS = sys.argv[1]
PORT = int(sys.argv[2])

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/eth/v1/beacon/genesis"):
            body = {"data": {"genesis_time": GENESIS, "genesis_validators_root": "0x" + "00" * 32, "genesis_fork_version": "0x00000000"}}
        elif self.path.startswith("/eth/v1/config/spec"):
            body = {"data": {"SECONDS_PER_SLOT": "2"}}
        else:
            self.send_response(404); self.end_headers(); return
        data = json.dumps(body).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", PORT), H).serve_forever()
