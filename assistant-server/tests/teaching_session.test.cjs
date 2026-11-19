const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const script = fs.readFileSync(path.join(__dirname,'../templates/teaching-session.js'),'utf8');
function environment(status = {is_bound:false,has_session:false}) {
  const dialogs = [], requests = [], window = {};
  const location = {origin:'https://assistant.example',pathname:'/ta',search:'?course=_c_1',hash:'#work',href:''};
  const document = {body:{append(dialog) {dialogs.push(dialog);}},createElement(tag) {
    assert.equal(tag,'dialog');
    const nodes = new Map(['[data-bind]','[data-once]','[data-cancel]'].map(key => [key,{style:{},listeners:{},
      addEventListener(name,fn) {this.listeners[name] = fn;}}]));
    return {style:{},listeners:{},removed:false,shown:false,
      setAttribute() {}, querySelector:key=>nodes.get(key),querySelectorAll:()=>[...nodes.values()],
      addEventListener(name,fn) {this.listeners[name] = fn;},showModal() {this.shown=true;},
      close() {this.listeners.close();},remove() {this.removed=true;}};
  }};
  vm.runInNewContext(script,{window,document,location,encodeURIComponent,fetch:async (url,options) => {
    requests.push({url,options}); return {ok:true,status:200,json:async()=>status};
  }});
  return {window,dialogs,requests,location};
}
const flush = () => new Promise(resolve=>setImmediate(resolve));

test('unbound users get binding, temporary login and cancel with the complete local return path', async () => {
  const env = environment();
  const result = env.window.TeachingSession.ensure(); await flush();
  assert.equal(env.dialogs.length,1); const dialog=env.dialogs[0]; assert.equal(dialog.shown,true);
  assert.equal(dialog.querySelector('[data-bind]').href,'/pkuvideofullclass?callback=%2Fta%3Fcourse%3D_c_1%23work');
  assert.equal(dialog.querySelector('[data-once]').href,'/teaching-login?next=%2Fta%3Fcourse%3D_c_1%23work');
  dialog.querySelector('[data-cancel]').listeners.click();
  assert.equal(await result,false); assert.equal(dialog.removed,true); assert.equal(env.location.href,'');
  assert.deepEqual(env.requests.map(r=>r.url),['/api/teaching-session']);
});

test('a persisted temporary cookie and a binding each skip the choice dialog', async () => {
  for (const status of [{is_bound:false,has_session:true},{is_bound:true,has_session:false}]) {
    const env=environment(status); assert.equal(await env.window.TeachingSession.ensure(),true);
    assert.equal(env.dialogs.length,0);
  }
});

test('authentication failure opens one dialog and Escape allows a later attempt', async () => {
  const env=environment();
  const first=env.window.TeachingSession.handleError('教学网登录已失效');
  const second=env.window.TeachingSession.handleError('尚未绑定教学网');
  assert.equal(first,second); assert.equal(env.dialogs.length,1);
  env.dialogs[0].close(); assert.equal(await first,false);
  const third=env.window.TeachingSession.handleError('一次性登录已失效');
  assert.equal(env.dialogs.length,2); env.dialogs[1].close(); await third;
  await env.window.TeachingSession.handleError('网络连接失败'); assert.equal(env.dialogs.length,2);
});

function pageScript(name) {
  return fs.readFileSync(path.join(__dirname, `../templates/${name}.html`), 'utf8')
    .match(/<script>([\s\S]*?)<\/script>/)[1];
}

function pageElements() {
  const elements = new Map();
  return id => {
    if (!elements.has(id)) {
      elements.set(id, {
        value: '', style: {}, hidden: true, options: [], querySelectorAll: () => [],
        add() {}, replaceChildren() {},
        addEventListener(event, handler) { this[event] = handler; }
      });
    }
    return elements.get(id);
  };
}

test('home prompts for teaching login only after clicking the assignment link', async () => {
  const getElementById = pageElements();
  const requests = [];
  let dialogs = 0;
  vm.runInNewContext(pageScript('home'), {
    document: {getElementById},
    window: {TeachingSession: {choose() { dialogs++; }}},
    async fetch(url) {
      requests.push(url);
      return {ok: true, json: async () => ({login_required: true, reminders: []})};
    }
  });
  await flush();
  assert.deepEqual(requests, ['/api/assignments']);
  assert.equal(dialogs, 0);
  assert.equal(getElementById('assignment-login').hidden, false);
  getElementById('assignment-login').click();
  assert.equal(dialogs, 1);
});

test('home updates the remaining days, hours and minutes without another fetch', async () => {
  const getElementById = pageElements();
  const end = Date.parse('2026-11-20T23:59:00+08:00');
  let now = end - (24 * 60 + 125) * 60000;
  let tick;
  let requests = 0;
  const spans = [];
  vm.runInNewContext(pageScript('home'), {
    document: {
      getElementById,
      createElement(tag) {
        const element = {classList: {toggle() {}}, append() {}};
        if (tag === 'span') spans.push(element);
        return element;
      }
    },
    window: {},
    Date: class extends Date { static now() { return now; } },
    setInterval(callback) { tick = callback; },
    async fetch() {
      requests++;
      return {ok: true, json: async () => ({reminders: [
        {title: '作业', course: '课程', completed: false, deadline: new Date(end).toISOString()}
      ]})};
    }
  });
  await flush();
  assert.equal(spans[0].textContent, '1天2小时5分钟');
  now += 60000;
  tick();
  assert.equal(spans[0].textContent, '1天2小时4分钟');
  now = end;
  tick();
  assert.equal(spans[0].textContent, '已截止');
  assert.equal(requests, 1);
});

test('replay loads stored lessons without checking the teaching session and prompts on refresh', async () => {
  const getElementById = pageElements();
  const requests = [];
  const errors = [];
  const courses = '/api/pkuvideofullclass/courses';
  const term = '26-27学年第1学期';
  const values = {
    '/api/site/features': {},
    [courses]: {success: true, courses: [{key: '_course_1', name: '缓存课程', term}]},
    [courses + '/_course_1/videos']: {lessons: [{id: 'cached', is_processed: true, tex_filename: 'note.tex'}]},
    [courses + '/refresh']: {error: '尚未绑定教学网'},
  };
  vm.runInNewContext(pageScript('pkuvideo_sequence'), {
    document: {getElementById, querySelectorAll: () => []},
    Option: class {},
    window: {
      siteAccount: Promise.resolve({username: 'student'}),
      addEventListener() {},
      TeachingSession: {handleError(message) { errors.push(message); }}
    },
    async fetch(url) {
      requests.push(url);
      assert.ok(Object.hasOwn(values, url), url);
      return {ok: !values[url].error, text: async () => JSON.stringify(values[url])};
    }
  });
  await flush();
  assert.deepEqual(requests, ['/api/site/features', courses, courses + '/_course_1/videos']);
  assert.match(getElementById('courseList').innerHTML, /缓存课程/);
  assert.match(getElementById('lessonList').innerHTML, /下载PDF/);
  assert.deepEqual(errors, []);
  await getElementById('refreshBtn').click();
  assert.deepEqual(errors, ['尚未绑定教学网']);
  assert.match(getElementById('lessonList').innerHTML, /下载PDF/);
});
