'use strict';
// Benchmark adapter, not a production static server or framework comparison.
const cluster = require('node:cluster');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const [mode, countText, root, manifest, portText] = process.argv.slice(2);
const count = Number(countText);
const port = Number(portText);

if (cluster.isPrimary) {
  // Explicit RR on Windows; distribution is per connection, not per request.
  cluster.schedulingPolicy = cluster.SCHED_RR;
  let stopping = false;
  let listening = 0;
  const children = [];
  const stop = () => {
    if (stopping) return;
    stopping = true;
    cluster.disconnect(() => process.exit(0));
    setTimeout(() => {
      for (const worker of Object.values(cluster.workers)) worker.kill();
    }, 5000).unref();
  };
  process.stdin.resume();
  process.stdin.on('end', stop);
  for (let index = 0; index < count; index++) {
    const worker = cluster.fork();
    children.push(worker.process.pid);
    worker.on('listening', () => {
      if (++listening === count) {
        fs.writeFileSync(manifest, JSON.stringify({primary: process.pid,
          workers: children, scheduling: 'SCHED_RR', mode}));
      }
    });
    worker.on('exit', (code) => {
      if (!stopping) {
        console.error(`unexpected worker exit: ${code}`);
        stop();
        process.exitCode = 1;
      }
    });
  }
} else {
  // A killed primary closes its IPC channel. Never leave benchmark children.
  process.on('disconnect', () => process.exit(0));
  const server = http.createServer(async (req, res) => {
    try {
      if (mode === 'not_found') {
        res.writeHead(404, {'Content-Length': 0});
        res.end();
        return;
      }
      if (mode === 'static') {
        // Fixed allowlist, no arbitrary path traversal in this measurement.
        const name = req.url === '/' ? 'index.html' :
          req.url === '/1m.bin' ? '1m.bin' : null;
        if (!name) { res.writeHead(404); res.end(); return; }
        const handle = await fs.promises.open(path.join(root, name), 'r');
        try {
          const stat = await handle.stat();
          res.writeHead(200, {'Content-Type': name.endsWith('.html') ?
            'text/html' : 'application/octet-stream', 'Content-Length': stat.size});
          const stream = handle.createReadStream({highWaterMark: 128 * 1024});
          stream.on('error', () => res.destroy());
          res.on('close', () => stream.destroy());
          stream.pipe(res);
        } catch (error) { await handle.close(); throw error; }
        return;
      }
      const chunks = [];
      let length = 0;
      for await (const chunk of req) {
        length += chunk.length;
        if (length > 1024 * 1024) { res.writeHead(413); res.end(); return; }
        chunks.push(chunk);
      }
      let body;
      if (req.url === '/cpu') {
        body = 'a'.repeat(64);
        for (let round = 0; round < 20000; round++) {
          body = crypto.createHash('sha256').update(body).digest('hex');
        }
      } else if (req.url === '/body') {
        body = crypto.createHash('sha256').update(Buffer.concat(chunks)).digest('hex');
      } else if (req.url === '/json') {
        body = JSON.stringify({message: 'Hello from PHP'});
      } else {
        body = 'Hello from PHP'; // Identical bytes to PHP, despite adapter language.
      }
      res.writeHead(200, {'Content-Type': req.url === '/json' ?
        'application/json' : 'text/plain; charset=utf-8',
        'Content-Length': Buffer.byteLength(body)});
      res.end(body);
    } catch (error) {
      console.error(error);
      if (!res.headersSent) res.writeHead(500);
      res.end();
    }
  });
  server.listen(port, '127.0.0.1');
}
