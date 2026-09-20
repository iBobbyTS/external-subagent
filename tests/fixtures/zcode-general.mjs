import readline from 'node:readline';
if (process.argv.includes('--version')) { console.log('0.16.5'); process.exit(0); }
const send = (value) => console.log(JSON.stringify(value));
let seq = 0;
const event = (type, payload = {}) => send({ method: 'session/event', params: {
  eventId: String(++seq), sessionId: 'fixture-session', seq, timestamp: seq, type, payload,
} });
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  const request = JSON.parse(line);
  let result = {};
  if (request.method === 'session/create') result = { session: { sessionId: 'fixture-session' } };
  if (request.method === 'session/subscribe') result = { subscribed: true };
  if (request.method === 'session/send') result = { turnId: 'fixture-turn' };
  if (request.id !== undefined) send({ id: request.id, result });
  if (request.method === 'session/send') {
    event('turn.started');
    event('turn.completed', { response: 'fixture complete' });
  }
});
