const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

const html = fs.readFileSync(path.join(__dirname, '../templates/admin.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

function harness() {
  const elements = new Map();
  function node(id) {
    if (!elements.has(id)) elements.set(id, {
      value: '', innerHTML: '', textContent: '', hidden: false, events: {},
      addEventListener(name, listener) { this.events[name] = listener; },
      querySelectorAll() { return []; },
      showModal() { this.open = true; },
      close(value) { this.open = false; this.returnValue = value; this.events.close?.(); },
    });
    return elements.get(id);
  }
  const snapshot = {
    current_user_id: 'admin', username: '管理员',
    users: Array.from({ length: 25 }, (_, i) => ({
      id: i === 0 ? 'admin' : `user-${i}`, username: i === 1 ? '<img src=x onerror=alert(1)>' : `用户${i}`,
      email: `user${i}@example.com`, is_admin: i === 0, is_active: i !== 3,
      storage_quota: 3, processing_quota: 5, processing_priority: i + 1, sessions: 1,
      created_at: '2026-11-01T08:00:00Z',
    })),
    whitelist: ['first@example.com'],
    jobs: [
      { id: 'notes-1', kind: 'notes', user_id: 'user-1', status: 'running', updated_at: '2026-11-13T08:00:00Z' },
      { id: 'replay-1', kind: 'replay', user_id: 'user-1', status: 'succeeded', updated_at: '2026-11-13T08:00:00Z' },
      { id: 'translation-1', kind: 'translation', user_id: 'user-1', status: 'queued', updated_at: '2026-11-13T08:00:00Z' },
    ],
  };
  const requests = [];
  const actionButton = {};
  const context = vm.createContext({
    document: { getElementById: node, querySelectorAll: () => [], querySelector: () => actionButton, addEventListener() {} },
    window: { addEventListener() {} }, location: { hash: '#index' }, history: { replaceState() {} },
    fetch: async (url, options) => {
      requests.push({ url, options });
      if (!options.method) return { ok: true, json: async () => structuredClone(snapshot) };
      assert.equal(options.headers['X-Admin-Request'], '1');
      const body = JSON.parse(options.body);
      if (url === '/api/admin/whitelist') snapshot.whitelist = [...new Set(body.emails)].sort();
      else {
        const id = url.split('/')[4];
        const user = snapshot.users.find(user => user.id === id);
        if (url.endsWith('/revoke-sessions')) user.sessions = 0;
        else Object.assign(user, body);
      }
      return { ok: true, json: async () => ({ success: true }) };
    },
  });
  vm.runInContext(script, context);
  const run = code => vm.runInContext(code, context);
  return { node, snapshot, requests, run };
}

test('entry is a model directory without dashboard or welcome widgets', async () => {
  const h = harness(); await h.run('fetchData()'); h.run('route()');
  const view = h.node('view').innerHTML;
  for (const model of ['users', 'whitelist', 'notes', 'replay', 'translation']) assert.ok(view.includes(`href="#${model}"`));
  assert.doesNotMatch(html, /overview-view|class="stats"|class="welcome"|linear-gradient/);
  assert.ok(h.requests.every(request => request.url === '/api/admin/data'));
});

test('lists filter, sort, paginate and escape user supplied text', async () => {
  const h = harness(); await h.run('fetchData()');
  h.run("model='users'; renderList()");
  assert.match(h.node('view').innerHTML, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.doesNotMatch(h.node('view').innerHTML, /<img src=x/);
  h.run("listState.users.filter='inactive'; renderList()");
  assert.match(h.node('view').innerHTML, /1 条用户记录/);
  assert.match(h.node('view').innerHTML, /user3@example.com/);
  h.run("listState.users.filter='all'; listState.users.role='admin'; renderList()");
  assert.match(h.node('view').innerHTML, /1 条用户记录/);
  h.run("listState.users.role='all'; listState.users.q='user2@'; renderList()");
  assert.match(h.node('view').innerHTML, /1 条用户记录/);
  h.run("listState.users.q=''; listState.users.sort='processing_priority'; listState.users.direction=-1; listState.users.page=2; listState.users.selected.add('user-24'); renderList()");
  assert.match(h.node('view').innerHTML, /25 条用户记录/);
  assert.equal(h.run('listState.users.selected.size'), 0, 'selection cannot leak from another page');
  assert.deepEqual(JSON.parse(h.run('JSON.stringify(filteredRecords().slice(0,2).map(user=>user.processing_priority))')), [25, 24]);
});

test('task models remain separate and queued tasks match the waiting filter', async () => {
  const h = harness(); await h.run('fetchData()');
  h.run("model='notes'; renderList()");
  assert.match(h.node('view').innerHTML, /notes-1/);
  assert.doesNotMatch(h.node('view').innerHTML, /replay-1|translation-1/);
  h.run("model='translation'; listState.translation.filter='pending'; renderList()");
  assert.match(h.node('view').innerHTML, /translation-1/);
  h.run("recordId='translation-1'; renderRecord()");
  assert.match(h.node('view').innerHTML, /任务状态由处理流程维护，此处只读/);
});

test('user editing and whitelist add/change preserve other records', async () => {
  const h = harness(); await h.run('fetchData()');
  await h.run("recordId='user-2'; saveUser({elements:{is_active:{checked:false},storage_quota:{value:'8'},processing_quota:{value:'9'},processing_priority:{value:'2'}}})");
  assert.equal(h.snapshot.users[2].processing_priority, 2);
  assert.equal(h.snapshot.users[2].is_active, false);
  assert.equal(h.snapshot.users[3].processing_priority, 4);
  await h.run("recordId='add'; saveEmail({elements:{email:{value:'second@example.com'}}})");
  await h.run('fetchData()');
  await assert.rejects(h.run("saveEmail({elements:{email:{value:'first@example.com'}}})"), /已在白名单/);
  await h.run("recordId='second@example.com'; saveEmail({elements:{email:{value:'renamed@example.com'}}})");
  assert.deepEqual(h.snapshot.whitelist, ['first@example.com', 'renamed@example.com']);
});

test('batch action requires confirmation and preserves administrator protection', async () => {
  const h = harness(); await h.run('fetchData()');
  h.run("model='users'; listState.users.selected.add('user-3')");
  h.node('action').value = 'enable'; h.node('action').selectedOptions = [{ textContent: '启用所选用户' }];
  let pending = h.run('runAction()');
  assert.equal(h.node('confirmation').open, true);
  h.node('confirmation').close('cancel'); await pending;
  assert.equal(h.snapshot.users[3].is_active, false);
  pending = h.run('runAction()'); h.node('confirmation').close('yes'); await pending;
  assert.equal(h.snapshot.users[3].is_active, true);
  assert.match(h.node('notice').textContent, /成功处理 1 条/);
  h.run("model='users'; listState.users.selected.add('admin')");
  h.node('action').value = 'disable';
  pending = h.run('runAction()'); h.node('confirmation').close('yes'); await pending;
  assert.equal(h.snapshot.users[0].is_active, true);
  assert.match(h.node('notice').textContent, /包含管理员/);
});
