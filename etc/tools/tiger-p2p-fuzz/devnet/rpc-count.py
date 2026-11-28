# JSON-RPC proxy that forwards to an upstream and logs each method name, one per line.
import http.server, json, sys, urllib.request
listen, upstream, log = int(sys.argv[1]), sys.argv[2], sys.argv[3]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers['content-length']))
        try:
            req = json.loads(body); reqs = req if isinstance(req, list) else [req]
            with open(log, 'a') as f:
                for r in reqs: f.write(r.get('method', '?') + '\n')
        except Exception: pass
        resp = urllib.request.urlopen(urllib.request.Request(upstream, body, {'content-type': 'application/json'})).read()
        self.send_response(200); self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(resp))); self.end_headers(); self.wfile.write(resp)
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(('127.0.0.1', listen), H).serve_forever()
