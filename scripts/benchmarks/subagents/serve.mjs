import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
const root = await fs.realpath(process.argv[2]);
const types = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.css': 'text/css' };
const server = http.createServer(async (request, response) => {
  try {
    if (request.method !== 'GET') { response.writeHead(405).end(); return; }
    const pathname = decodeURIComponent(new URL(request.url, 'http://localhost').pathname);
    let file = path.resolve(root, `.${pathname}`);
    if ((await fs.stat(file)).isDirectory()) file = path.join(file, 'index.html');
    file = await fs.realpath(file);
    if (!file.startsWith(root + path.sep)) { response.writeHead(403).end(); return; }
    response.writeHead(200, { 'content-type': types[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
    response.end(await fs.readFile(file));
  } catch { response.writeHead(404).end(); }
});
server.listen(0, '127.0.0.1', () => console.log(JSON.stringify({ port: server.address().port, root, pid: process.pid })));
