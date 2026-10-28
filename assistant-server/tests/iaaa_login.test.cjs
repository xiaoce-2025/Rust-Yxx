const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../templates/auth.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

function environment(search = '', api = async () => ({ok: true, text: async () => '{}'}), images = []) {
  const nodes = new Map(), calls = [];
  function node(id) {
    if (!nodes.has(id)) nodes.set(id, {
      hidden: id === 'iaaaView', value: '', disabled: false, sheet: {disabled: false},
      style: {}, classes: new Set(), attributes: {}, listeners: {}, textContent: '',
      classList: {toggle(name, active) {active ? node(id).classes.add(name) : node(id).classes.delete(name);}},
      addEventListener(event, fn) {this.listeners[event] = fn;},
      setAttribute(name, value) {this.attributes[name] = value;}, focus() {},
    });
    return nodes.get(id);
  }
  const location = {origin: 'https://assistant.example', search, href: ''};
  vm.runInNewContext(script, {
    document: {getElementById: node, querySelector: () => node('card'), querySelectorAll: () => images, body: node('body')},
    location, URL, URLSearchParams,
    fetch: async (url, options) => {
      if (url === '/api/auth/me') return {ok: false};
      calls.push({url, options});
      return api(url, options);
    },
  });
  return {node, location, calls};
}
const event = {preventDefault() {}};

test('IAAA and local account modes, including QR placeholder, switch without network requests', () => {
  const {node, calls} = environment();
  node('useIaaa').listeners.click();
  assert.equal(node('accountView').hidden, true);
  assert.equal(node('iaaaView').hidden, false);
  node('iaaaQrTab').listeners.click();
  assert.equal(node('iaaaForm').hidden, true);
  assert.equal(node('iaaaQrPanel').hidden, false);
  assert.equal(node('iaaaQrTab').attributes['aria-selected'], 'true');
  assert.match(html, /敬请期待/);
  node('iaaaAccountTab').listeners.click();
  assert.equal(node('iaaaForm').hidden, false);
  node('useAccount').listeners.click();
  assert.equal(node('accountView').hidden, false);
  assert.equal(node('loginForm').style.display, 'block');
  node('tabRegister').listeners.click();
  assert.equal(node('loginForm').style.display, 'none');
  assert.equal(node('registerForm').style.display, 'block');
  assert.equal(node('tabRegister').attributes['aria-selected'], 'true');
  node('tabLogin').listeners.click();
  assert.equal(node('loginForm').style.display, 'block');
  assert.equal(calls.length, 0);
});

test('IAAA submission preserves password, uses cookie session and redirects locally', async () => {
  const {node, calls, location} = environment('?next=%2Fsettings');
  node('iaaaUsername').value = ' 2400012345 ';
  node('iaaaPassword').value = ' password with spaces ';
  await node('iaaaForm').listeners.submit(event);
  assert.equal(calls[0].url, '/api/auth/iaaa');
  assert.equal(calls[0].options.credentials, 'include');
  assert.deepEqual(JSON.parse(calls[0].options.body), {username: '2400012345', password: ' password with spaces '});
  assert.equal(location.href, '/settings');
  assert.equal(node('iaaaPassword').value, '');
});

test('failed authentication displays the error, clears credentials and allows retry', async () => {
  const {node, location} = environment('', async () => ({ok: false, text: async () => '{"error":"白名单校验失败"}'}));
  node('iaaaPassword').value = 'secret';
  await node('iaaaForm').listeners.submit(event);
  assert.match(node('iaaaStatus').textContent, /白名单校验失败/);
  assert.equal(node('iaaaPassword').value, '');
  assert.equal(node('iaaaSubmit').disabled, false);
  assert.equal(location.href, '');
});

test('malformed and external redirect targets fall back to the app', async () => {
  for (const next of ['https://evil.example', '//evil.example', 'javascript:alert(1)', 'http://[']) {
    const {node, location} = environment('?next=' + encodeURIComponent(next));
    await node('iaaaForm').listeners.submit(event);
    assert.equal(location.href, '/app');
  }
});

test('full illustration stays hidden until decoding completes, including cached images', async () => {
  for (const cached of [false, true]) {
    let finishDecoding;
    const decoded = new Promise(resolve => {finishDecoding = resolve;});
    const classes = new Set(), handlers = {};
    const image = {complete: cached, naturalWidth: cached ? 1672 : 0,
      decode: () => decoded, classList: {add: value => classes.add(value)},
      addEventListener: (name, fn) => {handlers[name] = fn;}};
    environment('', undefined, [image]);
    assert.equal(classes.has('is-ready'), false);
    const loading = cached ? undefined : handlers.load();
    assert.equal(classes.has('is-ready'), false);
    finishDecoding();
    await loading;
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(classes.has('is-ready'), true);
  }
});

test('decoding failure keeps the full preview visible', async () => {
  const classes = new Set(), handlers = {};
  const image = {complete: false, naturalWidth: 0,
    decode: async () => {throw new Error('decode failed');},
    classList: {add: value => classes.add(value)},
    addEventListener: (name, fn) => {handlers[name] = fn;}};
  environment('', undefined, [image]);
  await handlers.load();
  assert.equal(classes.has('is-ready'), false);
});
