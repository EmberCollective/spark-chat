// Run with: node tests/browser-check.cjs (Playwright installed under target/ui-tools).
const { chromium } = require('../target/ui-tools/node_modules/playwright');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
const server = http.createServer((request, response) => {
  const url = new URL(request.url, 'http://localhost');
  if (url.pathname === '/') {
    const html = fs.readFileSync(path.join(root, 'web/index.html'), 'utf8')
      .replace('<script src="app.js" defer></script>', `<script src="/${url.searchParams.has('multi') ? 'ui-multi-check.js' : 'ui-check.js'}"></script><script src="app.js" defer></script>`);
    response.setHeader('Content-Type', 'text/html; charset=utf-8');
    response.end(html);
    return;
  }
  const name = path.basename(url.pathname);
  const file = ['ui-check.js','ui-multi-check.js'].includes(name) ? path.join(root, 'tests', name) : path.join(root, 'web', name);
  if (!fs.existsSync(file)) { response.writeHead(404).end(); return; }
  response.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'image/png');
  response.end(fs.readFileSync(file));
});
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const width of [1000, 320, 390]) {
      for (const resume of [false, true, 'multi']) {
        const page = await browser.newPage({ viewport: { width, height: 800 } });
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        await page.goto(`http://127.0.0.1:${server.address().port}/?${width < 600 ? 'touch&' : ''}${resume === 'multi' ? 'multi' : resume ? 'resume' : ''}`);
        await page.waitForSelector('#check-results');
        const result = await page.locator('#check-results').textContent();
        assert(!result.includes('FAIL'), `${width}/${resume}: ${result}`);
        assert.deepEqual(errors, []);
        console.log(`${width}px resume=${resume}: ${result.split('\n').at(-1)}`);
        await page.close();
      }
    }
  } finally { await browser.close(); server.close(); }
})().catch(error => { console.error(error); server.close(); process.exitCode = 1; });
