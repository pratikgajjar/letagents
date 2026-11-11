// Conformance check: `letagents mcp` MCP Events, driven by an independent client, the Vercel AI SDK (@ai-sdk/mcp).
// Webhook mode end to end: events/list, events/subscribe (verification challenge, Standard Webhooks signature checked
// by the SDK's createMCPEventWebhook), one real message, refresh, unsubscribe. Exits 1 on any failure.
//
//   node tests/aisdk/verify.mjs <letagents binary> <agent name> <shell command that sends a message to that agent>
import http from 'node:http';
import { experimental_createMCPClient as createMCPClient, experimental_createMCPEventWebhook as createMCPEventWebhook } from '@ai-sdk/mcp';
import { StdioMCPTransport } from '@ai-sdk/mcp/mcp-stdio';
import { execFileSync } from 'node:child_process';

const [bin, agent, sendCmd] = process.argv.slice(2);
const rows = new Map();
const store = {
  async get(k) { return rows.get(k); },
  async getById(id) { return [...rows.values()].find((s) => s.id === id); },
  async set(s) { rows.set(s.key, s); },
  async update(k, p) { rows.set(k, { ...rows.get(k), ...p }); },
  async delete(k) { rows.delete(k); },
};
let got;
const done = new Promise((r) => (got = r));
const hook = createMCPEventWebhook({ store, onEvent: async ({ subscription, event }) => got({ subscription, event }), onError: (e) => console.error('webhook error:', e) });
const srv = http.createServer(async (req, res) => {
  const chunks = []; for await (const c of req) chunks.push(c);
  const r = await hook(new Request(`http://127.0.0.1${req.url}`, { method: req.method, headers: req.headers, body: Buffer.concat(chunks) }));
  res.writeHead(r.status, Object.fromEntries(r.headers)); res.end(Buffer.from(await r.arrayBuffer()));
});
await new Promise((r) => srv.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${srv.address().port}/hook`;

const client = await createMCPClient({
  transport: new StdioMCPTransport({ command: bin, args: ['mcp', '--as', agent], env: { ...process.env, LETAGENTS_EVENTS_ALLOW_PRIVATE: '1' } }),
  experimental_events: { store },
});
const list = await client.experimental_events.list();
console.log('list:', list.events.map((e) => `${e.name} [${e.delivery}]`).join(', '));
const sub = await client.experimental_events.subscribe({ name: 'message.received', arguments: { include_text: true }, delivery: { mode: 'webhook', url, allowInsecureLocalhost: true }, ttlMs: 600000 });
console.log('subscribed:', JSON.stringify(sub));
const t0 = Date.now();
execFileSync('sh', ['-c', sendCmd], { stdio: 'inherit' });
const r = await Promise.race([done, new Promise((_, no) => setTimeout(() => no(new Error('no event in 30 s')), 30000))]);
console.log(`event after ${((Date.now() - t0) / 1000).toFixed(1)} s:`, JSON.stringify(r.event));
await new Promise((ok) => setTimeout(ok, 1000)); // let the 2xx reach the server
const ref = await client.experimental_events.refresh({ id: sub.id });
console.log('refresh:', JSON.stringify(ref));
const fail = (m) => { console.error('FAIL ' + m); process.exitCode = 1; };
if (list.events[0]?.name !== 'message.received') fail('events/list');
if (r.event.data.text === undefined || !r.event.eventId) fail('event payload');
if (ref.id !== sub.id) fail('refresh keeps the subscription id');
await client.experimental_events.unsubscribe({ id: sub.id });
console.log('unsubscribed');
await client.close(); srv.close();
