/* Functional test for the ACoworkWebSite Agent Hub.
   Runs the real agents.js inside jsdom against the real generated
   agents.json — no network, no build step, nothing deployed.

   Requires jsdom (dev-only, kept outside both repos):
     npm i jsdom@24 --prefix <tmpdir>
     NODE_PATH=<tmpdir>/node_modules node dev/agent-hub/hub.test.js <sitePath>

   <sitePath> is the ACoworkWebSite checkout — required, never defaulted: a
   hard-coded path would only ever work on the machine it was written on.
*/
'use strict';
const fs = require('fs');
const path = require('path');
const { JSDOM } = require('jsdom');

const SITE = process.argv[2];
if (!SITE) {
  console.error('usage: node dev/agent-hub/hub.test.js <sitePath>');
  console.error('  <sitePath> = the ACoworkWebSite checkout (reads agents/)');
  process.exit(2);
}
if (!fs.existsSync(path.join(SITE, 'agents/index.html'))) {
  console.error('not an ACoworkWebSite checkout: ' + SITE + ' has no agents/index.html');
  process.exit(2);
}
const html = fs.readFileSync(path.join(SITE, 'agents/index.html'), 'utf8')
  // Strip only the external scripts — the embedded JSON snapshot (the file://
  // fallback) must survive into the DOM for the fallback tests.
  .replace(/<script[^>]*\ssrc=[^>]*><\/script>/g, '');
const dataText = fs.readFileSync(path.join(SITE, 'agents/data/agents.json'), 'utf8');
const scriptSrc = fs.readFileSync(path.join(SITE, 'agents/agents.js'), 'utf8');

let pass = 0, fail = 0;
const ok = (name, cond, extra) => {
  if (cond) { pass++; console.log('  PASS  ' + name); }
  else { fail++; console.log('  FAIL  ' + name + (extra !== undefined ? '  -> ' + extra : '')); }
};

function boot(fetchImpl) {
  return new JSDOM(html, {
    url: 'http://localhost:8811/agents/index.html',
    runScripts: 'dangerously',
    pretendToBeVisual: true,
    beforeParse(w) { w.fetch = fetchImpl; },
  });
}

const okFetch = () => Promise.resolve({
  ok: true, status: 200, json: () => Promise.resolve(JSON.parse(dataText)),
});

const dom = boot(okFetch);
const { window } = dom;
const $ = (id) => window.document.getElementById(id);
const cards = () => Array.from($('ah-grid').querySelectorAll('.ah-card'));
const wait = (ms = 60) => new Promise((r) => setTimeout(r, ms));

/* --- real-browser helpers (Chrome headless) ---------------------------
   jsdom cannot model `!important`, so the cascade is verified in Chrome.
   The user's actual failure mode was "opened from disk and nothing
   rendered" — that is exactly what this pass reproduces. */

function findChrome() {
  const env = process.env.CHROME_PATH;
  if (env && fs.existsSync(env)) return env;
  const candidates = [
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
    'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
  ];
  return candidates.find((p) => fs.existsSync(p)) || null;
}

function runChrome(bin, relPath) {
  const os = require('os');
  const { pathToFileURL } = require('url');
  const profile = path.join(os.tmpdir(), 'ahub-chrome-' + process.pid + '-' + Date.now());
  const fileUrl = pathToFileURL(path.join(SITE, relPath)).href;
  const out = require('child_process').execFileSync(bin, [
    '--headless', '--disable-gpu', '--no-sandbox',
    '--user-data-dir=' + profile,
    '--virtual-time-budget=8000',
    '--dump-dom',
    fileUrl,
  ], {
    encoding: 'utf8', maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'ignore'],
    timeout: 90000,   // never let a stuck browser hang the suite
  });
  try { fs.rmSync(profile, { recursive: true, force: true }); } catch (e) { /* best effort */ }
  return out;
}

(async () => {
  // Watchdog: a stuck browser or socket must never hang the whole suite.
  const watchdog = setTimeout(() => {
    console.error('\nHARNESS TIMEOUT: aborting after 180s');
    process.exit(3);
  }, 180000);
  watchdog.unref();

  window.eval(scriptSrc);
  await wait(150);

  console.log('\n[1] initial render');
  ok('skeleton hidden after load', $('ah-skeleton').hidden === true);
  ok('grid visible', $('ah-grid').hidden === false);
  ok('7 cards rendered', cards().length === 7, cards().length);
  ok('hero count = 7', $('ah-count-total').textContent === '7', $('ah-count-total').textContent);
  ok('status says 7 agents', /7 agents/.test($('ah-status').textContent), $('ah-status').textContent);
  ok('error box hidden', $('ah-error').hidden === true);
  ok('empty box hidden', $('ah-empty').hidden === true);

  console.log('\n[2] card contract');
  const first = cards()[0];
  ok('name present', (first.querySelector('.ah-card__name').textContent || '').length > 0);
  ok('role + category present', /·/.test(first.querySelector('.ah-card__role').textContent), first.querySelector('.ah-card__role').textContent);
  ok('description present', first.querySelector('.ah-card__desc').textContent.length > 40);
  ok('no tier/official badge anywhere', window.document.querySelectorAll('.ah-tag').length === 0, window.document.querySelectorAll('.ah-tag').length);
  const defaultNames = cards().map((c) => c.querySelector('.ah-card__name').textContent.toLowerCase());
  ok('default order is by display name, not id prefix', defaultNames.join(',') === defaultNames.slice().sort().join(','), defaultNames.join(','));
  const pony = cards().find((c) => c.getAttribute('data-agent') === 'github.ponytail');
  ok('third-party agent present', !!pony);
  ok('author shown on third-party card', /by /.test(pony.querySelector('.ah-card__author').textContent), pony.querySelector('.ah-card__author').textContent);
  ok('author shown on every card', cards().every((c) => /by /.test(c.querySelector('.ah-card__author').textContent)));
  const dl = first.querySelector('a[download]');
  ok('download anchor has [download]', !!dl && dl.hasAttribute('download'));
  ok('download href points at package', /^downloads\/.+\.agent$/.test(dl.getAttribute('href')), dl.getAttribute('href'));
  ok('avatar img has lazy loading', !!first.querySelector('img.ah-card__avatar[loading="lazy"]'));

  console.log('\n[3] permissions on the card (R5)');
  ok('permissions visible on card', first.querySelectorAll('.ah-perm').length >= 3, first.querySelectorAll('.ah-perm').length);
  ok('high-risk styled', first.querySelectorAll('.ah-perm--risk').length >= 1);
  const arch = cards().find((c) => c.getAttribute('data-agent') === 'com.acowork.software-architect');
  ok('architect flags Shell', Array.from(arch.querySelectorAll('.ah-perm--risk')).some((p) => /Shell/.test(p.textContent)));
  const pm = cards().find((c) => c.getAttribute('data-agent') === 'com.acowork.product-manager');
  ok('PM does not flag Shell', !Array.from(pm.querySelectorAll('.ah-perm--risk')).some((p) => /Shell/.test(p.textContent)));

  console.log('\n[4] search (R2)');
  const q = $('ah-q');
  const type = async (v) => {
    q.value = v;
    q.dispatchEvent(new window.Event('input', { bubbles: true }));
    await wait(120);
  };

  await type('PRD');
  // "PRD" hits product-manager (capability prd_writing) AND project-manager
  // (capability description "…into a structured PRD") — capability prose is
  // deliberately searchable, this is the expected recall.
  ok('"PRD" -> product + project', cards().map((c) => c.getAttribute('data-agent')).sort().join(',') === 'com.acowork.product-manager,com.acowork.project-manager', cards().map((c) => c.getAttribute('data-agent')).join(','));
  ok('status "2 of 7"', /2 of 7/.test($('ah-status').textContent), $('ah-status').textContent);
  ok('clear button revealed', $('ah-clear').hidden === false);

  await type('regression');
  ok('"regression" -> QA only', cards().length === 1 && cards()[0].getAttribute('data-agent') === 'com.acowork.quality-assurance', cards().map((c) => c.getAttribute('data-agent')).join(','));

  await type('code review');
  ok('multi-word AND', cards().length === 2, cards().map((c) => c.getAttribute('data-agent')).join(','));

  await type('figma');
  ok('capability match "figma"', cards().length === 1 && cards()[0].getAttribute('data-agent') === 'com.acowork.product-manager');

  await type('prd-writing');
  ok('skill-name match', cards().length === 1);

  await type('ARCHITECT');
  ok('case-insensitive', cards().length >= 1, cards().length);

  await type('  PRD  ');
  ok('surrounding whitespace trimmed', cards().length === 2, cards().length);

  await type('zzzz-no-such-agent');
  ok('no hit -> grid hidden', $('ah-grid').hidden === true);
  ok('no hit -> empty state', $('ah-empty').hidden === false);
  ok('no hit -> status "0 of 7"', /0 of 7/.test($('ah-status').textContent), $('ah-status').textContent);

  console.log('\n[5] URL state + reset (R3)');
  ok('q written to URL', window.location.search.indexOf('q=zzzz') !== -1, window.location.search);
  $('ah-empty-reset').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  await wait(100);
  ok('reset restores 7 cards', cards().length === 7, cards().length);
  ok('URL cleared', window.location.search.indexOf('q=') === -1, window.location.search);
  ok('reset button hidden when idle', $('ah-reset').hidden === true);

  console.log('\n[6] category chips + facets');
  const chips = Array.from($('ah-cats').children);
  ok('chips = All + 5', chips.length === 6, chips.length);
  const counts = chips.map((c) => c.querySelector('.ah-chip__n').textContent).join(',');
  ok('facet counts correct (7,3,1,1,1,1)', counts === '7,3,1,1,1,1', counts);
  const eng = chips.find((c) => c.dataset.cat === 'engineering');
  eng.dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  await wait(80);
  ok('engineering -> 3 cards', cards().length === 3, cards().length);
  ok('chip active', eng.classList.contains('is-active'));
  ok('cat in URL', window.location.search.indexOf('cat=engineering') !== -1, window.location.search);
  await type('prd');
  ok('facets follow query', $('ah-cats').querySelector('[data-cat="product"] .ah-chip__n').textContent === '1');
  // The active chip must stay clickable even at zero hits — disabling it would
  // strand the user on an empty result with no way back.
  const engBtn = $('ah-cats').querySelector('[data-cat="engineering"]');
  ok('active zero-hit chip stays enabled', engBtn.disabled === false && engBtn.querySelector('.ah-chip__n').textContent === '0', engBtn.disabled);
  ok('inactive zero-hit chip is disabled', ($('ah-cats').querySelector('[data-cat="quality"]').disabled === true), $('ah-cats').querySelector('[data-cat="quality"]').disabled);
  $('ah-reset').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  await wait(100);

  console.log('\n[7] sort');
  const s = $('ah-sort');
  s.value = 'size'; s.dispatchEvent(new window.Event('change', { bubbles: true }));
  await wait(80);
  const sizes = cards().map((c) => {
    const t = c.querySelector('.ah-card__meta').textContent;
    const m = t.match(/(\d+(?:\.\d+)?)\s*(KB|MB|B)/);
    return m ? parseFloat(m[1]) * (m[2] === 'MB' ? 1024 : m[2] === 'KB' ? 1 : 0.001) : -1;
  });
  ok('size ascending', sizes.every((v, i) => i === 0 || sizes[i - 1] <= v), sizes.join(','));
  s.value = 'name'; s.dispatchEvent(new window.Event('change', { bubbles: true }));
  await wait(80);
  const names = cards().map((c) => c.querySelector('.ah-card__name').textContent.toLowerCase());
  ok('name A-Z', names.join(',') === names.slice().sort().join(','), names.join(','));
  s.value = 'category'; s.dispatchEvent(new window.Event('change', { bubbles: true }));
  await wait(80);
  const cats = cards().map((c) => c.querySelector('.ah-card__role').textContent.split('·')[1].trim());
  ok('category groups together', cats.join(',') === cats.slice().sort().join(','), cats.join(','));
  s.value = 'name'; s.dispatchEvent(new window.Event('change', { bubbles: true }));
  await wait(80);
  ok('no "featured" option remains', Array.from(s.options).every((o) => o.value !== 'featured'));

  console.log('\n[8] detail dialog (R4)');
  cards()[0].querySelector('.btn--ghost').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  await wait(80);
  const dlg = $('ah-detail');
  ok('showModal exists or dialog open', typeof dlg.showModal === 'function' || dlg.open === true, typeof dlg.showModal);
  ok('title filled', $('ah-detail-title').textContent.length > 0, $('ah-detail-title').textContent);
  const bodyTxt = $('ah-detail-body').textContent;
  ok('capabilities section', /Capabilities/.test(bodyTxt));
  ok('skills section', /Bundled skills/.test(bodyTxt));
  ok('sha-256 shown', /[0-9a-f]{64}/.test(bodyTxt), (bodyTxt.match(/[0-9a-f]{64}/) || ['none'])[0].slice(0, 10));
  ok('agent_id shown', /agent_id/.test(bodyTxt));
  ok('runtime version shown', /runtime/.test(bodyTxt));
  ok('high-impact warning present', /High-impact/.test(bodyTxt));
  ok('author shown in detail', /author/.test(bodyTxt));
  ok('no trust-tier wording in detail', !/Official|Community|not audited/i.test(bodyTxt), bodyTxt.slice(0, 120));
  ok('download href set', /^downloads\//.test($('ah-detail-download').getAttribute('href')));
  ok('copy link has absolute url', /^http:\/\/localhost:8811\/agents\/downloads\//.test($('ah-detail-download').dataset.url), $('ah-detail-download').dataset.url);
  ok('open= recorded in URL', window.location.search.indexOf('open=') !== -1, window.location.search);
  $('ah-detail-close').dispatchEvent(new window.MouseEvent('click', { bubbles: true }));
  await wait(60);
  ok('close removes open= from URL', window.location.search.indexOf('open=') === -1, window.location.search);

  console.log('\n[9] XSS: manifest strings must never become markup');
  const poisoned = JSON.parse(dataText);
  poisoned.agents[0].description = '<img src=x onerror="window.__pwned=1">';
  poisoned.agents[0].capabilities[0].key = '<svg onload="window.__pwned=2">';
  poisoned.agents[0].author = '"><script>window.__pwned=3<\/script>';
  const dom2 = boot(() => Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(poisoned) }));
  dom2.window.eval(scriptSrc);
  await wait(160);
  const g2 = dom2.window.document.getElementById('ah-grid');
  ok('no injected img', g2.querySelectorAll('img[src="x"]').length === 0);
  ok('no injected svg', g2.querySelectorAll('svg[onload]').length === 0);
  ok('no script executed', dom2.window.__pwned === undefined);
  ok('payload shown as literal text', /<img src=x/.test(g2.querySelector('.ah-card__desc').textContent));

  console.log('\n[10] failure paths + deep links');
  // (a) fetch fails but the embedded snapshot is present -> file:// fallback
  const dom3 = boot(() => Promise.reject(new TypeError('Failed to fetch')));
  dom3.window.eval(scriptSrc);
  await wait(200);
  const d3 = dom3.window.document;
  ok('fallback: grid rendered from snapshot', d3.getElementById('ah-grid').hidden === false);
  ok('fallback: 7 cards', d3.querySelectorAll('#ah-grid .ah-card').length === 7, d3.querySelectorAll('#ah-grid .ah-card').length);
  ok('fallback: error box NOT shown', d3.getElementById('ah-error').hidden === true);
  ok('fallback: offline notice shown', d3.getElementById('ah-notice').hidden === false);
  ok('fallback: notice explains how to fix', /http\.server|publish/.test(d3.getElementById('ah-notice').textContent), d3.getElementById('ah-notice').textContent);
  ok('fallback: search still works', (() => {
    const q = d3.getElementById('ah-q');
    q.value = 'regression';
    q.dispatchEvent(new dom3.window.Event('input', { bubbles: true }));
    return true;
  })());
  await wait(120);
  ok('fallback: search narrows to 1', d3.querySelectorAll('#ah-grid .ah-card').length === 1, d3.querySelectorAll('#ah-grid .ah-card').length);

  // (b) fetch fails and there is no snapshot -> real error state
  const bareHtml = html.replace(/<script id="ah-data"[^>]*>[\s\S]*?<\/script>/, '');
  const dom3b = new JSDOM(bareHtml, {
    url: 'http://localhost:8811/agents/index.html', runScripts: 'dangerously', pretendToBeVisual: true,
    beforeParse(w) { w.fetch = () => Promise.reject(new TypeError('Failed to fetch')); },
  });
  dom3b.window.eval(scriptSrc);
  await wait(200);
  const d3b = dom3b.window.document;
  ok('no snapshot + no fetch -> error state', d3b.getElementById('ah-error').hidden === false);
  ok('error: skeleton hidden', d3b.getElementById('ah-skeleton').hidden === true);
  ok('error: names the generator', /sync-agent-hub\.py/.test(d3b.getElementById('ah-error-detail').textContent));

  // (c) deep link restores state
  const dom4 = boot(okFetch);
  dom4.window.history.replaceState({}, '', '/agents/index.html?open=github.ponytail&q=ponytail&cat=engineering');
  dom4.window.eval(scriptSrc);
  await wait(200);
  ok('deep link restores query', dom4.window.document.getElementById('ah-q').value === 'ponytail', dom4.window.document.getElementById('ah-q').value);
  ok('deep link restores category', dom4.window.document.querySelector('[data-cat="engineering"]').classList.contains('is-active'));
  const d4 = dom4.window.document.getElementById('ah-detail-title');
  ok('deep link opens detail', d4.textContent === 'Ponytail', d4.textContent);

  console.log('\n[11] dialog avatar reuse (no detached-node crash)');
  const mixed = JSON.parse(dataText);
  mixed.agents.forEach((a) => { a.avatar = null; });          // no avatar at all
  const dom5 = boot(() => Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(mixed) }));
  dom5.window.eval(scriptSrc);
  await wait(160);
  const d5 = dom5.window.document;
  const W5 = dom5.window;
  // Scope to the live grid: #ah-skeleton also carries .ah-card, and it is what
  // the page shows before data arrives.
  const grid5 = d5.getElementById('ah-grid');
  grid5.querySelectorAll('.ah-card')[0].querySelector('.btn--ghost').dispatchEvent(new W5.MouseEvent('click', { bubbles: true }));
  await wait(60);
  ok('initials fallback shown when avatar missing', d5.getElementById('ah-detail-initial').hidden === false);
  ok('avatar img hidden', d5.getElementById('ah-detail-avatar').style.display === 'none');
  d5.getElementById('ah-detail-close').dispatchEvent(new W5.MouseEvent('click', { bubbles: true }));
  await wait(40);
  let threw = null;
  try {
    grid5.querySelectorAll('.ah-card')[1].querySelector('.btn--ghost').dispatchEvent(new W5.MouseEvent('click', { bubbles: true }));
  } catch (e) { threw = e; }
  ok('second open after fallback does not throw', threw === null, threw && threw.message);
  ok('dialog still has a title on second open', d5.getElementById('ah-detail-title').textContent.length > 0, d5.getElementById('ah-detail-title').textContent);
  ok('cards fall back to initials tile', grid5.querySelectorAll('.ah-card__avatar--initial').length === 7, grid5.querySelectorAll('.ah-card__avatar--initial').length);

  console.log('\n[12] embedded snapshot matches agents.json');
  const m = html.match(/<script id="ah-data"[^>]*>([\s\S]*?)<\/script>/);
  ok('snapshot block present in the page', !!m);
  const snap = JSON.parse(m[1]);
  const live = JSON.parse(dataText);
  ok('same agent count', snap.agents.length === live.agents.length, snap.agents.length + ' vs ' + live.agents.length);
  ok('same agent ids in the same order',
    JSON.stringify(snap.agents.map((a) => a.agent_id)) === JSON.stringify(live.agents.map((a) => a.agent_id)));
  ok('same sha256 per package',
    JSON.stringify(snap.agents.map((a) => a.package.sha256)) === JSON.stringify(live.agents.map((a) => a.package.sha256)));
  ok('payload cannot close the script tag early', m[1].indexOf('</script>') === -1);

  console.log('\n[13] stylesheet guard for the `hidden` attribute');
  const css = fs.readFileSync(path.join(SITE, 'styles.css'), 'utf8');
  // jsdom's getComputedStyle cannot model !important, so the cascade is
  // verified in a real browser below. Here we only assert the rule exists:
  // without it, `.ah-grid { display:grid }` beats the UA `[hidden]` rule and
  // hidden grids keep painting.
  ok('global [hidden] display:none rule present',
    /\[hidden\]\s*\{[^}]*display:\s*none\s*!important/.test(css), 'no [hidden] rule in styles.css');

  console.log('\n[14] real browser (Chrome headless) over file://');
  const chrome = findChrome();
  if (!chrome) {
    console.log('  SKIP  Chrome not found — set CHROME_PATH to enable');
  } else {
    const domFile = runChrome(chrome, 'agents/index.html');
    ok('grid is visible', /id="ah-grid"(?![^>]*hidden)/.test(domFile));
    ok('7 cards painted', (domFile.match(/class="ah-card"/g) || []).length === 7, (domFile.match(/class="ah-card"/g) || []).length);
    ok('error panel not shown', /id="ah-error"[^>]*hidden/.test(domFile));
    ok('skeleton grid hidden', /id="ah-skeleton"[^>]*hidden/.test(domFile));
    ok('offline notice shown', /id="ah-notice"(?![^>]*hidden)/.test(domFile));
  }

  console.log('\n[15] real browser over http:// (the deployed case)');
  const chromeHttp = findChrome();
  if (!chromeHttp) {
    console.log('  SKIP  Chrome not found — set CHROME_PATH to enable');
  } else {
    const http = require('http');
    const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.json': 'application/json', '.css': 'text/css', '.jpg': 'image/jpeg', '.png': 'image/png', '.woff2': 'font/woff2' };
    const server = http.createServer((req, res) => {
      const rel = decodeURIComponent(req.url.split('?')[0]).replace(/^\/+/, '') || 'index.html';
      // path.resolve normalizes separators — a raw startsWith against SITE
      // would fail on Windows ("E:\..." vs "E:/...") and 404 everything.
      const root = path.resolve(SITE);
      const file = path.resolve(root, rel);
      if (!file.startsWith(root + path.sep) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
        res.writeHead(404, { 'Connection': 'close' }); res.end('not found: ' + rel); return;
      }
      res.writeHead(200, {
        'Content-Type': MIME[path.extname(file)] || 'application/octet-stream',
        // Chrome's --dump-dom waits for network idle; a kept-alive socket
        // never goes idle, so the connection must be closed per response.
        'Connection': 'close',
      });
      res.end(fs.readFileSync(file));
    });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const port = server.address().port;

    const os = require('os');
    const profile = path.join(os.tmpdir(), 'ahub-chrome-http-' + process.pid + '-' + Date.now());
    let domHttp = '';
    try {
      domHttp = await new Promise((resolve, reject) => {
        // Async spawn on purpose: execFileSync would block this process's
        // event loop, and the static server lives in this process — Chrome's
        // request would never be answered (deadlock).
        const child = require('child_process').spawn(chromeHttp, [
          '--headless', '--disable-gpu', '--no-sandbox',
          '--user-data-dir=' + profile,
          '--virtual-time-budget=8000',
          '--dump-dom',
          'http://127.0.0.1:' + port + '/agents/index.html',
        ], { stdio: ['ignore', 'pipe', 'ignore'] });
        let out = '';
        child.stdout.on('data', (d) => { out += d; });
        const killer = setTimeout(() => child.kill(), 90000);
        child.on('close', () => { clearTimeout(killer); resolve(out); });
        child.on('error', (e) => { clearTimeout(killer); reject(e); });
      });
    } catch (e) {
      console.log('  SKIP  http pass failed: ' + (e.message || e).toString().split('\n')[0]);
    } finally {
      server.close();
      try { fs.rmSync(profile, { recursive: true, force: true }); } catch (e) { /* best effort */ }
    }

    ok('7 cards over http', (domHttp.match(/class="ah-card"/g) || []).length === 7, (domHttp.match(/class="ah-card"/g) || []).length);
    ok('grid visible over http', /id="ah-grid"(?![^>]*hidden)/.test(domHttp));
    ok('no error panel over http', /id="ah-error"[^>]*hidden/.test(domHttp));
    ok('no offline notice over http (live index used)', /id="ah-notice"[^>]*hidden/.test(domHttp));
  }

  console.log('\n' + '='.repeat(50));
  console.log('PASS ' + pass + '   FAIL ' + fail);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('HARNESS ERROR:', e); process.exit(2); });
