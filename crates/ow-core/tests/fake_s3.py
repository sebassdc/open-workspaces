#!/usr/bin/env python3
"""Credential-free S3 HTTP fixture for the AWS SDK transfers.

Serves signed path-style requests from ROOT/<bucket>/<key> and logs each to
ROOT/calls.jsonl. Prints its port on stdout. Fault files in ROOT: deny-head,
fail-manifest, put-budget (how many more chunk uploads succeed before the rest
are refused), no-versioning and unsafe-lifecycle. HEAD reports length, ETag and
(when asked) SHA-256; ranged GETs must match that ETag. Bucket versioning,
lifecycle (seven-day undo) and, when ROOT/write-once-policy exists, the
write-once policy for root/store/v1/ are answered as a safe deployment would.
"""
import base64
import hashlib
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlparse
from xml.sax.saxutils import escape

root = Path(sys.argv[1])
PAGE = 2  # small pages exercise continuation
budget_lock = threading.Lock()


def etag(data):
    return '"' + hashlib.sha256(data).hexdigest()[:32] + '"'


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def target(self):
        url = urlparse(self.path)
        query = parse_qs(url.query, keep_blank_values=True)
        assert (self.headers['Authorization'] or '').startswith('AWS4-HMAC-SHA256'), 'unsigned request'
        bucket, _, key = unquote(url.path).lstrip('/').partition('/')
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(['http', self.command, key, bucket]) + '\n')
        return bucket, key, query

    def reply(self, status, body=b'', headers=(), length=None):
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header('Content-Length', str(len(body) if length is None else length))
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(body)

    def error(self, status, code):
        self.reply(status, f'<Error><Code>{code}</Code><Message>bucket and key</Message></Error>'.encode())

    def do_HEAD(self):
        bucket, key, _ = self.target()
        if (root / 'deny-head').exists():
            return self.reply(403)
        path = root / bucket / key
        if not path.is_file():
            return self.reply(404)
        data = path.read_bytes()
        headers = [('ETag', etag(data))]
        if self.headers['x-amz-checksum-mode'] == 'ENABLED':
            headers.append(('x-amz-checksum-sha256', base64.b64encode(hashlib.sha256(data).digest()).decode()))
        self.reply(200, headers=headers, length=len(data))

    def do_GET(self):
        bucket, key, query = self.target()
        if 'versioning' in query:
            status = 'Suspended' if (root / 'no-versioning').exists() else 'Enabled'
            return self.reply(200, f'<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>{status}</Status></VersioningConfiguration>'.encode())
        if 'lifecycle' in query:
            expire = '<Expiration><Days>30</Days></Expiration>' if (root / 'unsafe-lifecycle').exists() else ''
            return self.reply(200, f'<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ID>undo</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status>{expire}<NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration></Rule></LifecycleConfiguration>'.encode())
        if 'policy' in query:
            if not (root / 'write-once-policy').exists():
                return self.error(404, 'NoSuchBucketPolicy')
            return self.reply(200, json.dumps({'Statement': [{'Effect': 'Deny', 'Principal': '*', 'Action': 's3:PutObject', 'Resource': f'arn:aws:s3:::{bucket}/root/store/v1/*', 'Condition': {'Null': {'s3:if-none-match': 'true'}, 'Bool': {'s3:ObjectCreationOperation': 'true'}}}]}).encode())
        if query.get('list-type') == ['2']:
            prefix = query.get('prefix', [''])[0]
            base = root / bucket
            keys = sorted(str(f.relative_to(base)) for f in base.rglob('*') if f.is_file()) if base.exists() else []
            keys = [k for k in keys if k.startswith(prefix)]
            start = int(query.get('continuation-token', ['0'])[0])
            page = keys[start:start + PAGE]
            more = start + PAGE < len(keys)
            contents = ''.join(f'<Contents><Key>{escape(k)}</Key><LastModified>2026-10-18T00:00:00.000Z</LastModified><ETag>"x"</ETag><Size>1</Size><StorageClass>STANDARD</StorageClass></Contents>' for k in page)
            token = f'<NextContinuationToken>{start + PAGE}</NextContinuationToken>' if more else ''
            body = f'<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>{bucket}</Name><Prefix>{escape(prefix)}</Prefix><KeyCount>{len(page)}</KeyCount><MaxKeys>{PAGE}</MaxKeys><IsTruncated>{str(more).lower()}</IsTruncated>{contents}{token}</ListBucketResult>'
            return self.reply(200, body.encode())
        path = root / bucket / key
        if not path.is_file():
            return self.error(404, 'NoSuchKey')
        data = path.read_bytes()
        if self.headers['Range']:
            if self.headers['If-Match'] != etag(data):
                return self.error(412, 'PreconditionFailed')
            start, end = map(int, self.headers['Range'].removeprefix('bytes=').split('-'))
            return self.reply(206, data[start:end + 1])
        self.reply(200, data)

    def do_PUT(self):
        bucket, key, _ = self.target()
        body = self.rfile.read(int(self.headers['Content-Length']))
        once = self.headers['If-None-Match'] == '*'
        checksum = base64.b64encode(hashlib.sha256(body).digest()).decode()
        if self.headers['x-amz-checksum-sha256'] != checksum:
            return self.error(400, 'BadDigest')
        if (root / 'fail-manifest').exists() and key.endswith('manifest.json'):
            return self.error(403, 'AccessDenied')
        budget = root / 'put-budget'
        with budget_lock:
            if '/chunks/' in f'/{key}' and budget.exists():
                left = int(budget.read_text())
                if left <= 0:
                    return self.error(403, 'AccessDenied')
                budget.write_text(str(left - 1))
        dest = root / bucket / key
        dest.parent.mkdir(parents=True, exist_ok=True)
        try:
            with dest.open('xb' if once else 'wb') as output:
                output.write(body)
        except FileExistsError:
            return self.error(412, 'PreconditionFailed')
        # Process-death fixture: bytes are durable, but the uploader has no response yet.
        if '/chunks/' in f'/{key}' and (root / 'pause-after-put').exists():
            (root / 'put-paused').write_text(key)
            while (root / 'pause-after-put').exists():
                time.sleep(0.01)
        self.reply(200, headers=[('ETag', etag(body))])


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
