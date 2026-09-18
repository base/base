#!/usr/bin/env python3
"""Test HA bootstrap against a scripted JSON-RPC server, without Docker or Raft."""

import json
import os
from pathlib import Path
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class ConductorSetupTest(unittest.TestCase):
    def run_setup(self, *, reject_join=False, incomplete_membership=False):
        state = {"polls": 0, "proxy_polls": 0, "voters": ["sequencer-0"], "early_join": False}

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                response = {"jsonrpc": "2.0", "id": request["id"]}
                method = request["method"]
                if method == "conductor_leader":
                    state["polls"] += 1
                    response["result"] = state["polls"] >= 5
                elif method == "conductor_addServerAsVoter":
                    state["early_join"] |= state["polls"] < 5
                    if reject_join or state["early_join"]:
                        response["error"] = {"code": -32000, "message": "node is not the leader"}
                    else:
                        state["voters"].append(request["params"][0])
                        response["result"] = None
                elif method == "conductor_clusterMembership":
                    voters = state["voters"][:1] if incomplete_membership else state["voters"]
                    response["result"] = {"servers": [{"id": voter} for voter in voters]}
                elif method == "optimism_rollupConfig":
                    state["proxy_polls"] += 1
                    if state["proxy_polls"] < 2:
                        response["error"] = {"code": -32000, "message": "sequencer not ready"}
                    else:
                        response["result"] = {"l2_chain_id": 84538453}
                else:
                    response["error"] = {"code": -32601, "message": "unknown method"}
                body = json.dumps(response).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            url = f"http://127.0.0.1:{server.server_port}"
            try:
                result = subprocess.run(
                    ["bash", str(Path(__file__).with_name("setup-conductor.sh"))],
                    env=dict(os.environ, CONDUCTOR0_URL=url, CONDUCTOR1_URL=url, CONDUCTOR2_URL=url),
                    capture_output=True, text=True, timeout=15,
                )
            finally:
                server.shutdown()
                thread.join()
        return result, state

    def test_waits_for_leader_and_ready_proxy(self):
        result, state = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(state["early_join"])
        self.assertEqual(state["voters"], ["sequencer-0", "sequencer-1", "sequencer-2"])
        self.assertGreaterEqual(state["proxy_polls"], 2)

    def test_join_rpc_errors_fail_setup(self):
        result, _state = self.run_setup(reject_join=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("node is not the leader", result.stderr)

    def test_incomplete_membership_fails_setup(self):
        result, _state = self.run_setup(incomplete_membership=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected three Raft voters", result.stderr)


if __name__ == "__main__":
    unittest.main()
