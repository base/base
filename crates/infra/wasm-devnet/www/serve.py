import http.server
import os
import socketserver
import functools

class H(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        super().end_headers()
    def log_message(self, *a): pass

handler = functools.partial(H, directory=os.path.dirname(os.path.abspath(__file__)))

class ReuseAddrServer(socketserver.TCPServer):
    allow_reuse_address = True

with ReuseAddrServer(('', 8765), handler) as s:
    print('Serving on http://localhost:8765 (COOP/COEP enabled)')
    s.serve_forever()
