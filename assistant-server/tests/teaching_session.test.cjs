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
