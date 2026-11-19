const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../templates/auth.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

function environment(search = '', api = async () => ({ok: true, text: async () => '{}'}), images = [], pathname = '/auth/admin') {
  const nodes = new Map(), calls = [];
  function node(id) {
    if (!nodes.has(id)) nodes.set(id, {
      hidden: id === 'iaaaQrPanel', value: '', disabled: false, sheet: {disabled: false},
      style: {}, classes: new Set(), attributes: {}, listeners: {}, textContent: '',
      classList: {toggle(name, active) {active ? node(id).classes.add(name) : node(id).classes.delete(name);}},
      addEventListener(event, fn) {this.listeners[event] = fn;},
      setAttribute(name, value) {this.attributes[name] = value;}, focus() {},
    });
    return nodes.get(id);
  }
  const location = {origin: 'https://assistant.example', search, pathname, href: ''};
  vm.runInNewContext(script.replace('const teachingCsrf = "";', 'const teachingCsrf = "test-csrf";'), {
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

test('IAAA is the only local login entry and QR remains a placeholder', () => {
  const {node, calls} = environment();
  assert.match(html, /id="iaaaView" class="main auth-layout">/);
  assert.doesNotMatch(html, /accountView|useAccount|loginForm|registerForm|\/api\/auth\/(login|register)/);
  const entry = fs.readFileSync(path.join(__dirname, '../templates/auth_entry.html'), 'utf8');
  assert.match(entry, /通过北京大学IAAA(?:统一)?认证登录/);
  node('iaaaQrTab').listeners.click();
  assert.equal(node('iaaaForm').hidden, true);
  assert.equal(node('iaaaQrPanel').hidden, false);
  assert.equal(node('iaaaQrTab').attributes['aria-selected'], 'true');
  assert.match(html, /敬请期待/);
  node('iaaaAccountTab').listeners.click();
  assert.equal(node('iaaaForm').hidden, false);
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

test('malformed and external redirect targets fall back to the homepage', async () => {
  for (const next of ['https://evil.example', '//evil.example', 'javascript:alert(1)', 'http://[']) {
    const {node, location} = environment('?next=' + encodeURIComponent(next));
    await node('iaaaForm').listeners.submit(event);
    assert.equal(location.href, '/');
  }
});

test('too many IAAA attempts keep the button disabled and block repeat form submission', async () => {
  for (const pathname of ['/auth/admin', '/teaching-login']) {
    const {node, calls, location} = environment('', async () => ({
      ok: false, text: async () => JSON.stringify({error:'IAAA 登录次数过多，请稍后再试'})
    }), [], pathname);
    node('iaaaPassword').value = 'secret';
    await node('iaaaForm').listeners.submit(event);
    assert.equal(node('iaaaSubmit').disabled, true);
    assert.match(node('iaaaStatus').textContent, /次数过多/);
    assert.equal(node('iaaaPassword').value, '');
    node('iaaaQrTab').listeners.click();
    node('iaaaAccountTab').listeners.click();
    await node('iaaaForm').listeners.submit(event);
    assert.equal(calls.length, 1);
    assert.equal(location.href, '');
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


test('temporary teaching login uses the existing form without creating a system login or retaining inputs', async () => {
  const {node, calls, location} = environment('?next=%2Fta%3Fcourse%3D_c_1', undefined, [], '/teaching-login');
  node('iaaaUsername').value = ' 2400012345 ';
  node('iaaaPassword').value = ' temporary secret ';
  await node('iaaaForm').listeners.submit(event);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/teaching-session');
  assert.equal(calls[0].options.headers['X-Teaching-CSRF'], 'test-csrf');
  assert.deepEqual(JSON.parse(calls[0].options.body), {username:'2400012345',password:' temporary secret '});
  assert.equal(location.href, '/ta?course=_c_1');
  assert.equal(node('iaaaPassword').value, '');
  assert.equal(node('iaaaUsername').value, '');
});

test('temporary teaching failure clears both inputs and does not follow an external return URL', async () => {
  const {node, calls, location} = environment('?next=https%3A%2F%2Fevil.example',
    async () => ({ok:false,text:async () => '{"error":"教学网登录失败"}'}), [], '/teaching-login');
  node('iaaaUsername').value = '2400012345'; node('iaaaPassword').value = 'secret';
  await node('iaaaForm').listeners.submit(event);
  assert.equal(calls[0].url, '/api/teaching-session');
  assert.equal(node('iaaaPassword').value, ''); assert.equal(node('iaaaUsername').value, '');
  assert.equal(node('iaaaSubmit').disabled, false); assert.equal(location.href, '');
  assert.match(node('iaaaStatus').textContent, /教学网登录失败/);
});
