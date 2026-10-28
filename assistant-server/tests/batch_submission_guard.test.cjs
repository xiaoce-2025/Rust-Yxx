const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../templates/batch_submission_guard.html'), 'utf8');
const script = html.match(/<script>([\s\S]*)<\/script>/)[1];
const STATUS = '/api/pkuvideofullclass/batch-submit/status';
const START = '/api/pkuvideofullclass/batch-submit';
const flush = () => new Promise(resolve => setImmediate(resolve));
function response(value, status = 200) {
  return {status, ok: status >= 200 && status < 300, json: async () => structuredClone(value), clone() { return response(value,status); }};
}
function environment(fetcher, page='/settings') {
  const calls = [], events = [], docListeners = new Map(), windowListeners = new Map(), nodes = new Map();
  const timers = new Map();
  let timerId=0;
  let document;
  class Element {
    constructor(id) {this.id=id;this.children=[];this.style={};this.attributes=new Map();this.inert=false;this.textContent='';this.innerHTML='';}
    setAttribute(k,v) {this.attributes.set(k,v);}
    toggleAttribute(k,on) {if(on)this.attributes.set(k,'');else this.attributes.delete(k);}
    appendChild(child) {this.children.push(child);nodes.set(child.id,child);}
    contains(child) {return child===this || this.children.includes(child);}
    focus() {document.activeElement=this;}
  }
  document = {body:new Element('body'),documentElement:new Element('root'),activeElement:null,
    createElement: () => new Element(''),
    getElementById: id => {if(!nodes.has(id))nodes.set(id,new Element(id));return nodes.get(id);},
    addEventListener: (name,handler) => {if(!docListeners.has(name))docListeners.set(name,[]);docListeners.get(name).push(handler);}
  };
  const app = new Element('app');document.body.appendChild(app);
  const storage = new Map();
  const window = {
    fetch: async (input,options) => {const url=new URL(typeof input==='string'?input:input.url,'https://assistant.example');calls.push(url.pathname);return fetcher(url.pathname,options);},
    addEventListener: (name,handler) => {if(!windowListeners.has(name))windowListeners.set(name,[]);windowListeners.get(name).push(handler);},
    dispatchEvent: event => {events.push(event);for(const handler of windowListeners.get(event.type)||[])handler(event);return true;}
  };
  vm.runInNewContext(script,{window,document,URL,Date,location:{href:'https://assistant.example'+page,origin:'https://assistant.example'},
    localStorage:{setItem:(k,v)=>storage.set(k,v),removeItem:k=>storage.delete(k)},
    CustomEvent:class {constructor(type,options){this.type=type;this.detail=options.detail;}},
    setInterval:(fn,delay)=>{const id=++timerId;timers.set(id,{fn,delay});return id;},clearInterval:id=>timers.delete(id),console
  });
  return {window,document,app,calls,events,docListeners,nodes,timers,
    tick:async()=>{for(const {fn,delay} of [...timers.values()])if(delay===2000)await fn();await flush();}};
}

test('reopening a page restores the non-dismissible mask before substantive APIs run', async () => {
  let status={active:true,id:'batch-1',total:3,completed:1,submitted:1,failed:0,estimated_remaining_seconds:25};
  const env=environment(async route=>response(route===STATUS?status:{ok:true}));
  await flush();
  assert.equal(env.window.BatchSubmission.active,true);
  assert.equal(env.nodes.get('globalBatchSubmissionOverlay').style.display,'grid');
  assert.match(env.nodes.get('globalBatchMessage').textContent,/任务正在提交中，请不要离开本页面，预计还需要\d+s提交完成/);
  assert.equal(env.app.inert,true);
  let prevented=false;
  for(const handler of env.docListeners.get('keydown'))handler({key:'Escape',preventDefault(){prevented=true;},stopImmediatePropagation(){}});
  assert.equal(prevented,true);
  assert.equal(env.window.BatchSubmission.active,true);
  const pending=env.window.fetch('/api/user/api-config');
  await flush();
  assert.equal(env.calls.includes('/api/user/api-config'),false);
  status={...status,active:false,status:'completed',completed:3};
  await env.window.BatchSubmission.refresh();
  await pending;
  assert.equal(env.nodes.get('globalBatchSubmissionOverlay').style.display,'none');
  assert.equal(env.app.inert,false);
  assert.equal(env.events.some(event=>event.type==='batchsubmissioncomplete'),true);
});

test('an idle status racing with batch start cannot dismiss its mask', async () => {
  let status={active:false};
  let accept;
  const acceptance=new Promise(resolve=>{accept=resolve;});
  const env=environment(async route=>route===START?acceptance:response(status));
  await env.window.BatchSubmission.ready;
  const pending=env.window.BatchSubmission.start({course_id:'course'});
  await flush();
  assert.equal(env.window.BatchSubmission.active,true);
  await env.window.BatchSubmission.refresh();
  assert.equal(env.window.BatchSubmission.active,true);
  status={active:true,id:'batch-2',total:2,estimated_remaining_seconds:40};
  accept(response({success:true,batch:status}));
  await pending;
  const calls=env.calls.length;
  await assert.rejects(env.window.fetch('/api/pkuvideofullclass/courses'),/任务正在提交中/);
  assert.equal(env.calls.length,calls);
  status={active:false,id:'batch-2',status:'completed'};
  await env.window.BatchSubmission.refresh();
  assert.equal(env.window.BatchSubmission.active,false);
});

test('server lock response activates other tabs and a network error never unlocks it', async () => {
  let offline=false;
  const env=environment(async route=>{
    if(route===STATUS){if(offline)throw new Error('offline');return response({active:false});}
    return response({submission:{active:true,id:'batch-other-tab',estimated_remaining_seconds:20}},423);
  });
  await env.window.BatchSubmission.ready;
  const blocked=await env.window.fetch('/api/user/api-config');
  assert.equal(blocked.status,423);
  assert.equal(env.window.BatchSubmission.active,true);
  offline=true;
  await env.window.BatchSubmission.refresh();
  assert.equal(env.window.BatchSubmission.active,true);
  const calls=env.calls.length;
  await assert.rejects(env.window.fetch('/api/pkuvideo/jobs'),/任务正在提交中/);
  assert.equal(env.calls.length,calls);
});

test('login page never polls task status, including refresh and cross-tab notifications', async () => {
  const env=environment(async()=>response({}), '/auth?next=/settings');
  await env.window.BatchSubmission.ready;
  await env.tick();
  await env.window.BatchSubmission.refresh();
  env.window.dispatchEvent({type:'storage',key:'assistant.batch-submission.active',newValue:'active'});
  await env.tick();
  assert.deepEqual(env.calls,[]);
  assert.equal(env.window.BatchSubmission.active,false);
  await env.window.fetch('/api/auth/iaaa');
  await env.tick();
  assert.deepEqual(env.calls,['/api/auth/iaaa']);
});

test('unauthorized status cancels polling and successful login resumes it', async () => {
  let authenticated=false;
  const env=environment(async route=>route===STATUS?response({active:false},authenticated?200:401):response({}));
  await env.window.BatchSubmission.ready;
  assert.equal(env.calls.filter(p=>p===STATUS).length,1);
  for(let i=0;i<3;i++)await env.tick();
  await env.window.BatchSubmission.refresh();
  assert.equal(env.calls.filter(p=>p===STATUS).length,1);
  authenticated=true;
  await env.window.fetch('/api/auth/login');
  await flush();
  await env.tick();
  assert.equal(env.calls.filter(p=>p===STATUS).length,3);
  await env.window.fetch('/api/auth/logout');
  const count=env.calls.length;
  await env.tick();
  assert.equal(env.calls.length,count);
});

test('unauthorized business request stops subsequent status polling', async () => {
  const env=environment(async route=>response({active:false},route===STATUS?200:401));
  await env.window.BatchSubmission.ready;
  await env.window.fetch('/api/user/api-config');
  const count=env.calls.length;
  await env.tick();
  assert.equal(env.calls.length,count);
});
