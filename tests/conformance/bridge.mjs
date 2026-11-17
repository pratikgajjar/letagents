// Streamable HTTP front for a stdio MCP server, so the official conformance runner (which addresses a URL)
// can test `letagents mcp`. Test-only: it adds no protocol logic, it only maps the transport, as SEP-3415
// describes the two transports:
//
//   POST {request}                -> one line on the server's stdin
//   server line with the same id  -> the HTTP response (JSON), or the last SSE frame of a stream
//   events/stream, subscriptions/listen
//                                 -> an SSE response; every notification whose
//                                    _meta["io.modelcontextprotocol/subscriptionId"] is the request id
//                                    is a `data:` frame on it
//   client aborts that response   -> notifications/cancelled {requestId} on stdin (the stdio cancel)
//   POST {notification}           -> stdin, 202
//
// It does not check the Mcp-Name routing header (SEP-3415 "Routing Headers"): that rule is for servers on
// Streamable HTTP, and letagents speaks stdio only, so tests/e2e.sh counts those rows as not applicable.
//
//   node bridge.mjs <port file> <server command> [args...]
import http from 'node:http';
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import readline from 'node:readline';

const [portFile, cmd, ...args] = process.argv.slice(2);
const child = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'inherit'] });
child.on('exit', (code) => {
  console.error(`bridge: server exited (${code})`);
  process.exit(1);
});
const send = (msg) => child.stdin.write(JSON.stringify(msg) + '\n');

const STREAMS = new Set(['events/stream', 'subscriptions/listen']);
const SUB_ID = 'io.modelcontextprotocol/subscriptionId';
/** In-flight requests by JSON id: { res, stream, open } */
const pending = new Map();
const key = (id) => JSON.stringify(id);

const json = (res, status, body) => {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
};
const frame = (p, msg) => {
  if (!p.open) {
    p.res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    p.open = true;
  }
  p.res.write(`data: ${JSON.stringify(msg)}\n\n`);
};

readline.createInterface({ input: child.stdout }).on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.id !== undefined && msg.method === undefined) {
    const p = pending.get(key(msg.id));
    if (!p) return; // cancelled: the client is gone
    pending.delete(key(msg.id));
    if (p.open) {
      frame(p, msg);
      p.res.end();
    } else {
      json(p.res, 200, msg);
    }
    return;
  }
  const sid = msg.params?._meta?.[SUB_ID];
  const p = sid === undefined ? undefined : pending.get(key(sid));
  if (p?.stream) frame(p, msg);
});

const server = http.createServer((req, res) => {
  if (req.method !== 'POST') return json(res, 405, { error: 'POST only' });
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    let msg;
    try {
      msg = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    } catch {
      return json(res, 400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } });
    }
    if (msg.id === undefined || msg.id === null) {
      if (msg.method) send(msg);
      res.writeHead(202).end();
      return;
    }
    const p = { res, stream: STREAMS.has(msg.method), open: false };
    pending.set(key(msg.id), p);
    res.on('close', () => {
      if (pending.get(key(msg.id)) === p) {
        pending.delete(key(msg.id));
        send({ jsonrpc: '2.0', method: 'notifications/cancelled', params: { requestId: msg.id, reason: 'client closed the HTTP request' } });
      }
    });
    send(msg);
  });
});
server.listen(0, '127.0.0.1', () => writeFileSync(portFile, String(server.address().port)));
