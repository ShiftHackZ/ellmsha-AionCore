import readline from 'node:readline';
const input = readline.createInterface({ input: process.stdin, terminal: false });
const reply = (id, result) => process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`);
input.on('line', (line) => {
  const message = JSON.parse(line);
  if (message.method === 'initialize') {
    reply(message.id, { protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'sd07-codex-fixture', version: '1.2.3' } });
  } else if (message.method === 'tools/list') {
    reply(message.id, { tools: [{ name: 'safe_echo', description: 'Return text unchanged.', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } }] });
  } else if (message.method === 'tools/call') {
    reply(message.id, { content: [{ type: 'text', text: message.params?.arguments?.text ?? '' }] });
  }
});
