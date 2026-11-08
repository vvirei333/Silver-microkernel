#!/usr/bin/env python3
# Local form/POST target for Wolfser's Ctrl+G flow.
#
# Start it on the host (the QEMU slirp guest reaches it as 10.0.2.2):
#   python3 tools/form-test-server.py
# In the guest:
#   wolfser http://10.0.2.2:8080/form.html
#   Ctrl+G, type a value, Enter
# The server logs the POST headers/body and replies with the body in the page.
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs
import sys
class H(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        print(fmt % args, flush=True)
    def do_GET(self):
        if self.path.startswith('/form'):
            body=b'''<!doctype html><html><body><h1>LOCAL FORM</h1>
<form action="/submit" method="post">
<input type="hidden" name="token" value="abc123">
<input type="text" name="q" value="cats">
<input type="submit" value="Go">
</form></body></html>'''
            self.send_response(200); self.send_header('Content-Type','text/html'); self.send_header('Set-Cookie','sid=42; Path=/'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()
    def do_POST(self):
        n=int(self.headers.get('Content-Length','0'))
        data=self.rfile.read(n).decode('utf-8','replace')
        print('POST', self.path, 'CL=', n, 'COOKIE=', self.headers.get('Cookie'), 'HEADERS', dict(self.headers), 'BODY', repr(data), flush=True)
        body=(f'<html><body><h1>POST OK</h1><p>{data}</p></body></html>').encode()
        self.send_response(200); self.send_header('Content-Type','text/html'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
HTTPServer(('0.0.0.0',8080),H).serve_forever()
