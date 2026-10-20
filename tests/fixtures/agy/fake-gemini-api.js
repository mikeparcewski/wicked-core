// Fake Gemini API for the agy seat's tests (no real account, no real key): answers agy's
// `GOOGLE_GEMINI_BASE_URL` requests on 127.0.0.1:$PORT. Derived from the agy spike's fakegemini.js. Test-only.
// The main model's reply is chosen by a word in the prompt:
//   AGY-TOOL   first turn asks for `run_command` ($FAKE_CMD), then answers after its result
//   AGY-SLOW   waits 60 s before answering (exercises --print-timeout)
//   AGY-FAIL   answers HTTP 500 (a model error)
//   otherwise  answers "VERDICT: PASS" plus the newest marker after AGY-ECHO=, if any (a
//              resumed conversation's request carries the earlier turns' too)
// Every request is logged to $SRV_LOG as one JSON line (method, url, key header present, length,
// whether it carries a tool result, and the markers it contains); `{"ready":true}` once listening.
const http = require('http'), fs = require('fs');
const LOG = process.env.SRV_LOG || '/tmp/fake-gemini.log';
const CMD = process.env.FAKE_CMD || 'echo agy-tool-ran > agy-tool.txt';
const usage = { promptTokenCount: 100, candidatesTokenCount: 5, cachedContentTokenCount: 40, thoughtsTokenCount: 3, totalTokenCount: 108 };
const sse = (res, obj) => { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.end('data: ' + JSON.stringify(obj) + '\r\n\r\n'); };
const text = (t) => ({ candidates: [{ content: { role: 'model', parts: [{ text: t }] }, finishReason: 'STOP' }], usageMetadata: usage });
http.createServer((req, res) => {
  let body = ''; req.on('data', (c) => body += c);
  req.on('end', () => {
    const hasFR = body.includes('"functionResponse"');
    const markers = (body.match(/AGY-[A-Z]+(=[A-Za-z0-9-]+)?/g) || []).filter((m, i, a) => a.indexOf(m) === i);
    fs.appendFileSync(LOG, JSON.stringify({ m: req.method, url: req.url, key: !!req.headers['x-goog-api-key'], len: body.length, hasFR, markers }) + '\n');
    if (req.method === 'GET') { res.writeHead(200, { 'content-type': 'application/json' }); return res.end('{"models":[]}'); }
    if (req.url.includes('flash-lite')) return sse(res, text('Agy Seat Test'));
    const prompt = body;
    if (prompt.includes('AGY-FAIL')) { res.writeHead(500, { 'content-type': 'application/json' }); return res.end('{"error":{"code":500,"message":"fake: model error","status":"INTERNAL"}}'); }
    if (prompt.includes('AGY-SLOW')) return setTimeout(() => sse(res, text('too late')), 60000);
    if (prompt.includes('AGY-TOOL') && !hasFR) return sse(res, { candidates: [{ content: { role: 'model', parts: [{ functionCall: { name: 'run_command', args: { CommandLine: CMD, Cwd: process.env.FAKE_CWD || '.', WaitMsBeforeAsync: 2000, toolSummary: 'write marker', toolAction: 'run' } } }] }, finishReason: 'STOP' }], usageMetadata: usage });
    const echoes = [...prompt.matchAll(/AGY-ECHO=([A-Za-z0-9-]+)/g)];
    const echo = echoes.length ? echoes[echoes.length - 1][1] : '';
    return sse(res, text('VERDICT: PASS ' + echo));
  });
}).listen(+(process.env.PORT || 8089), '127.0.0.1', () => fs.appendFileSync(LOG, '{"ready":true}\n'));
