const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

async function checkPage(name) {
  const html = fs.readFileSync(path.join(__dirname, `../templates/${name}.html`), 'utf8');
  const elements = new Map([...html.matchAll(/id="([^"]+)"/g)].map(([, id]) => [id, {
    value: '', style: {}, dataset: {}, classList: {add() {}, remove() {}},
    addEventListener(event, handler) { this[event] = handler; }
  }]));
  const requests = [];
  let detectionError = false;
  const context = vm.createContext({
    document: { getElementById(id) { assert.ok(elements.has(id), `Missing element: ${id}`); return elements.get(id); } },
    window: {},
    fetch: async (url, options) => {
      requests.push({url, body: options.body && JSON.parse(options.body)});
      if (url === '/api/note-credits/me') return {ok: true, json: async () => ({balance: 2})};
      if (url === '/api/pkuvideo/standalone-jobs') return {ok: true, text: async () => '[]'};
      const payload = url.endsWith('/check-m3u8')
        ? (detectionError ? {error: 'playlist unavailable'} : {is_m3u8: true, key_uri: 'https://example.invalid/key'})
        : {job_id: 'job-1', status: 'pending'};
      return {ok: true, text: async () => JSON.stringify(payload)};
    },
    setInterval() { return 1; }, clearInterval() {}, clearTimeout() {}, TextEncoder, URL, URLSearchParams, location: {}, console
  });
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1]
    .replace('checkLogin().then(() => openLessonReplay());', '')
    .replace('checkLogin().then(loadHistory);', '');
  if (name === 'pkuvideo') {
    vm.runInContext(fs.readFileSync(path.join(__dirname, '../templates/note-credits.js'), 'utf8'), context);
  }
  vm.runInContext(script, context);
  elements.get('videoUrl').value = 'https://example.invalid/video.m3u8';
  await vm.runInContext('checkM3u8AndSubmit()', context);
  assert.equal(elements.get('keyModal').style.display, 'flex');
  assert.deepEqual(requests[0].body, {video_url: 'https://example.invalid/video.m3u8'});
  elements.get('manualKey').value = '0123456789abcdef';
  elements.get('keyConfirmBtn').click();
  await new Promise(resolve => setImmediate(resolve));
  const submission = requests.find(r => /\/submit(?:-web)?$/.test(r.url));
  assert.deepEqual(submission.body, {video_url: 'https://example.invalid/video.m3u8', manual_key: '0123456789abcdef'});
  requests.length = 0;
  detectionError = true;
  await vm.runInContext('checkM3u8AndSubmit()', context);
  assert.match(elements.get('status').textContent, /playlist unavailable/);
  assert.ok(!requests.some(r => /\/submit(?:-web)?$/.test(r.url)));
  requests.length = 0;
  elements.get('videoUrl').value = 'https://example.invalid/video.mp4';
  await vm.runInContext('checkM3u8AndSubmit()', context);
  assert.deepEqual(requests.find(r => /\/submit(?:-web)?$/.test(r.url)).body, {video_url: 'https://example.invalid/video.mp4'});
  requests.length = 0;
  const key = 'hex:000102030405060708090a0b0c0d0e0f';
  elements.get('videoUrl').value = 'https://example.invalid/video.m3u8?token=a%20b%2Fc&enc_key=' + encodeURIComponent(key) + '&v=1';
  await vm.runInContext('checkM3u8AndSubmit()', context);
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(!requests.some(r => r.url.endsWith('/check-m3u8')), 'embedded key must skip manual key detection');
  assert.deepEqual(requests.find(r => /\/submit(?:-web)?$/.test(r.url)).body, {video_url: 'https://example.invalid/video.m3u8?token=a%20b%2Fc&v=1', manual_key: key});
  requests.length = 0;
  elements.get('videoUrl').value = 'https://example.invalid/video.m3u8?enc_key=short';
  await vm.runInContext('checkM3u8AndSubmit()', context);
  assert.equal(requests.length, 0);
  assert.match(elements.get('status').textContent, /enc_key/);
  elements.get('videoUrl').value = 'https://example.invalid/video.m3u8?enc_key=0123456789ABCDEF';
  assert.equal(vm.runInContext('readVideoInput().key', context), '0123456789ABCDEF');
  elements.get('videoUrl').value = 'https://example.invalid/video.m3u8?enc_key=0123456789abcdef&enc_key=0123456789abcdef';
  await vm.runInContext('checkM3u8AndSubmit()', context);
  assert.equal(requests.length, 0);
  assert.match(elements.get('status').textContent, /重复/);

}
(async () => {
  await checkPage('pkuvideo');
  await checkPage('pkuvideo_fw');
  console.log('Credential-free manual submission: key detection, key submission, MP4 and detection failure passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
