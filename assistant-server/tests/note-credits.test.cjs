const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');
const templates=path.join(__dirname,'../templates');
const source=fs.readFileSync(path.join(templates,'note-credits.js'),'utf8');
function fixture(balance,fail=false) {
  let checks=0,submissions=0;
  const elements=new Map();
  const element=()=>({style:{},children:[],textContent:'',disabled:false,append(...children){this.children.push(...children)},setAttribute(){},addEventListener(){},querySelector(tag){return this.children.find(x=>x.tag===tag)},showModal(){this.open=true},close(){this.open=false}});
  const document={createElement(tag){return {...element(),tag}},body:element(),querySelectorAll(){return []},getElementById(id){if(!elements.has(id)) elements.set(id,element());return elements.get(id)},addEventListener(){}};
  const context=vm.createContext({document,window:{},console,Set,fetch:async()=>{checks++;return {ok:!fail,json:async()=>({balance})}},readVideoInput:()=>({url:'https://example.invalid/video'}),api:async()=>{submissions++;return {job_id:'job',success:true}},statusEl:{},startPolling(){},updateQuotaDisplay(){},activeJobId:null});
  vm.runInContext(source,context);
  return {context,document,checks:()=>checks,submissions:()=>submissions};
}
function loadDirect(f) {
  const html=fs.readFileSync(path.join(templates,'pkuvideo.html'),'utf8');
  vm.runInContext(html.slice(html.indexOf('    let creditSubmitting'),html.indexOf('    async function checkM3u8AndSubmit')),f.context);
}
function loadLesson(f) {
  Object.assign(f.context,{isProcessing:false,selectedCourse:'course',activeLessons:new Set(),currentLessons:[{id:'lesson',is_processed:false}],renderLessonList(){},pollProcessStatus(){},setProcessing(value){f.context.isProcessing=value;}});
  const html=fs.readFileSync(path.join(templates,'pkuvideo_sequence.html'),'utf8');
  vm.runInContext(html.slice(html.indexOf('    async function handleProcessLesson'),html.indexOf('    async function handleDownloadPdf')),f.context);
}
for(const balance of [0,-3]) {
  test(`direct and lesson submissions stop for balance ${balance}`,async()=>{
    for(const kind of ['direct','lesson']) {
      const f=fixture(balance);kind==='direct'?loadDirect(f):loadLesson(f);
      await vm.runInContext(kind==='direct'?'submitJob()':"handleProcessLesson('lesson',null,true)",f.context);
      assert.equal(f.submissions(),0);assert.equal(f.checks(),1);
      const dialog=f.document.body.children[0];assert.equal(dialog.open,true);
      assert.match(dialog.querySelector('p').textContent,new RegExp(String(balance)));
    }
  });
}
test('balance API errors stop submissions',async()=>{
  for(const kind of ['direct','lesson']) {
    const f=fixture(1,true);kind==='direct'?loadDirect(f):loadLesson(f);
    await vm.runInContext(kind==='direct'?'submitJob()':"handleProcessLesson('lesson')",f.context);
    assert.equal(f.submissions(),0);assert.match(f.document.body.children[0].querySelector('h2').textContent,/暂时无法获取额度/);
  }
});
test('rapid direct and lesson clicks submit once without reserving credits',async()=>{
  for(const kind of ['direct','lesson']) {
    const f=fixture(1);kind==='direct'?loadDirect(f):loadLesson(f);
    await vm.runInContext(kind==='direct'?'Promise.all([submitJob(),submitJob(),submitJob()])':"Promise.all([handleProcessLesson('lesson'),handleProcessLesson('lesson')])",f.context);
    assert.equal(f.submissions(),1);assert.equal(f.checks(),1);
  }
});
test('batch checks once and admits five lessons with one credit',async()=>{
  const f=fixture(1);loadLesson(f);
  f.context.currentLessons=Array.from({length:5},(_,id)=>({id:String(id),is_processed:false}));
  let starts=0;
  f.context.window.BatchSubmission={active:false,start:async()=>{starts++}};
  f.context.processAllBtnEl={addEventListener:(_,handler)=>{f.context.batchClick=handler}};
  const html=fs.readFileSync(path.join(templates,'pkuvideo_sequence.html'),'utf8');
  vm.runInContext(html.slice(html.indexOf("    processAllBtnEl.addEventListener('click'"),html.indexOf("    window.addEventListener('batchsubmissioncomplete'")),f.context);
  await vm.runInContext('Promise.all([batchClick(),batchClick()])',f.context);
  assert.equal(starts,1);assert.equal(f.checks(),1);
});
