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
    fetch: async (url, options) => {
      requests.push({url, body: options.body && JSON.parse(options.body)});
      const payload = url.endsWith('/check-m3u8')
        ? (detectionError ? {error: 'playlist unavailable'} : {is_m3u8: true, key_uri: 'https://example.invalid/key'})
        : {job_id: 'job-1', status: 'pending'};
      return {ok: true, text: async () => JSON.stringify(payload)};
    },
    setInterval() { return 1; }, clearInterval() {}, TextEncoder, URLSearchParams, location: {}, console
  });
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1]
    .replace('checkLogin().then(() => openLessonReplay());', '')
    .replace(/    checkLogin\(\);/, '');
  vm.runInContext(script, context);
  if (name === 'pkuvideo') vm.runInContext('processingQuota = 2;', context);
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
  assert.deepEqual(requests[0].body, {video_url: 'https://example.invalid/video.mp4'});
}
(async () => {
  await checkPage('pkuvideo');
  await checkPage('pkuvideo_fw');
  console.log('Credential-free manual submission: key detection, key submission, MP4 and detection failure passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
