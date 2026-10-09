import { createServer, request } from 'node:http';
import { createHash, randomBytes } from 'node:crypto';

// A controllable Responses upstream. It implements wire protocols, not the
// audit service; every assertion still traverses the real gateway and plugin.
export function eventsFor(body, id) {
  const text = body.generate === false ? '' : 'Agent 方案：先检查数据库，再执行迁移。😀';
  const item = { id: `msg_${id}`, type: 'message', role: 'assistant', phase: 'final_answer', status: 'completed', content: [{ type: 'output_text', text, annotations: [] }] };
  const response = { id, object: 'response', created_at: Math.floor(Date.now() / 1000), model: body.model || 'gpt-5.4', status: 'completed', output: text ? [item] : [], usage: { input_tokens: 3, output_tokens: 4, total_tokens: 7, input_tokens_details: { cached_tokens: 0 } } };
  return [
    { type: 'response.created', response: { ...response, status: 'in_progress', output: [] } },
    ...(text ? [
      { type: 'response.output_item.added', output_index: 0, item: { ...item, status: 'in_progress', content: [] } },
      { type: 'response.content_part.added', item_id: item.id, output_index: 0, content_index: 0, part: { type: 'output_text', text: '', annotations: [] } },
      { type: 'response.output_text.delta', response_id: id, item_id: item.id, output_index: 0, content_index: 0, delta: text.slice(0, 12) },
      { type: 'response.output_text.delta', response_id: id, item_id: item.id, output_index: 0, content_index: 0, delta: text.slice(12) },
      { type: 'response.output_text.done', response_id: id, item_id: item.id, output_index: 0, content_index: 0, text },
      { type: 'response.output_item.done', output_index: 0, item },
    ] : []),
    { type: 'response.completed', response },
  ].map((event, sequence_number) => ({ ...event, sequence_number }));
}

function frame(payload, opcode = 1, masked = false) {
  const data = Buffer.from(payload), size = data.length;
  const header = Buffer.alloc(size < 126 ? 2 : size < 65536 ? 4 : 10);
  header[0] = 0x80 | opcode;
  header[1] = (masked ? 0x80 : 0) | (size < 126 ? size : size < 65536 ? 126 : 127);
  if (size >= 126 && size < 65536) header.writeUInt16BE(size, 2);
  else if (size >= 65536) header.writeBigUInt64BE(BigInt(size), 2);
  if (!masked) return Buffer.concat([header, data]);
  const mask = randomBytes(4), encoded = Buffer.from(data);
  for (let index = 0; index < encoded.length; index++) encoded[index] ^= mask[index % 4];
  return Buffer.concat([header, mask, encoded]);
}

function peer(socket, initial = Buffer.alloc(0), client = false) {
  let pending = initial, fragments = [], fragmentOpcode;
  const messages = [], waiters = [];
  const deliver = (message) => waiters.length ? waiters.shift().resolve(message) : messages.push(message);
  const parse = () => {
    while (pending.length >= 2) {
      const final = !!(pending[0] & 0x80), opcode = pending[0] & 15, masked = !!(pending[1] & 0x80);
      let size = pending[1] & 127, offset = 2;
      if (size === 126) { if (pending.length < 4) return; size = pending.readUInt16BE(2); offset = 4; }
      if (size === 127) { if (pending.length < 10) return; size = Number(pending.readBigUInt64BE(2)); offset = 10; }
      if (size > 16 * 1024 * 1024) { socket.destroy(new Error('Test WebSocket frame exceeds limit')); return; }
      if (pending.length < offset + (masked ? 4 : 0) + size) return;
      const mask = masked ? pending.subarray(offset, offset + 4) : null;
      if (masked) offset += 4;
      const payload = Buffer.from(pending.subarray(offset, offset + size));
      pending = pending.subarray(offset + size);
      if (mask) for (let index = 0; index < payload.length; index++) payload[index] ^= mask[index % 4];
      if (opcode === 9) { socket.write(frame(payload, 10, client)); continue; }
      if (opcode === 10) continue;
      if (opcode === 8) { socket.end(frame(payload, 8, client)); continue; }
      if (opcode) fragmentOpcode = opcode;
      fragments.push(payload);
      if (final) {
        const complete = Buffer.concat(fragments); fragments = [];
        if (fragmentOpcode === 1) deliver(JSON.parse(complete.toString('utf8')));
      }
    }
  };
  socket.on('data', (data) => { pending = Buffer.concat([pending, data]); parse(); });
  socket.on('error', (error) => { while (waiters.length) waiters.shift().reject(error); });
  socket.on('close', () => { while (waiters.length) waiters.shift().reject(new Error('WebSocket closed before terminal response')); });
  parse();
  return {
    send: (value) => socket.write(frame(JSON.stringify(value), 1, client)),
    close: () => socket.end(frame(Buffer.alloc(0), 8, client)),
    receive: () => messages.length ? Promise.resolve(messages.shift()) : new Promise((resolve, reject) => {
      const waiter = { resolve: (value) => { clearTimeout(timer); resolve(value); }, reject: (error) => { clearTimeout(timer); reject(error); } };
      const timer = setTimeout(() => { const index = waiters.indexOf(waiter); if (index >= 0) waiters.splice(index, 1); reject(new Error('WebSocket response timed out')); }, 20000);
      waiters.push(waiter);
    }),
  };
}

export async function connectWebSocket(url, headers = {}) {
  const target = new URL(url); target.protocol = target.protocol === 'wss:' ? 'https:' : 'http:';
  return new Promise((resolve, reject) => {
    const req = request(target, { headers: { ...headers, connection: 'Upgrade', upgrade: 'websocket', 'sec-websocket-version': '13', 'sec-websocket-key': randomBytes(16).toString('base64') } });
    req.on('upgrade', (_, socket, initial) => resolve(peer(socket, initial, true)));
    req.on('response', (response) => reject(new Error(`WebSocket rejected: ${response.statusCode}`)));
    req.on('error', reject); req.end();
  });
}

export async function startUpstream(port = 0) {
  let sequence = 0;
  const requests = [];
  const upgraded = new Set();
  const server = createServer(async (req, res) => {
    if (req.method !== 'POST' || !req.url.endsWith('/responses')) {
      res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ data: [] })); return;
    }
    const parts = []; for await (const part of req) parts.push(part);
    const body = JSON.parse(Buffer.concat(parts).toString('utf8')); requests.push(body);
    const events = eventsFor(body, `resp_integration_${++sequence}`);
    if (body.stream === false) { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(events.at(-1).response)); return; }
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    for (const event of events) res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
    res.end('data: [DONE]\n\n');
  });
  server.on('upgrade', (req, socket, initial) => {
    upgraded.add(socket); socket.on('close', () => upgraded.delete(socket));
    const accept = createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    const connection = peer(socket, initial);
    (async () => { for (;;) { const body = await connection.receive(); requests.push(body); for (const event of eventsFor(body, `resp_integration_${++sequence}`)) connection.send(event); } })().catch(() => socket.destroy());
  });
  await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}`, requests, close: () => new Promise((resolve) => { for (const socket of upgraded) socket.destroy(); server.closeAllConnections(); server.close(resolve); }) };
}
