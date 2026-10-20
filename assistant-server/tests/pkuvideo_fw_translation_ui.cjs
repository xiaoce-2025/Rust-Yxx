// Developer-only logic checks using Node's built-ins. No browser/server dependency.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

class Element {
  constructor(id) {
    this.id = id; this.style = {}; this.dataset = {}; this.events = {};
    this.children = []; this.value = ''; this.textContent = ''; this.innerHTML = '';
    this.currentTime = 42; this.duration = 600; this.paused = false;
    this.playbackRate = 1.25; this.volume = 0.4; this.muted = true; this.loads = 0;
    this.track = { mode: 'disabled' };
  }
  addEventListener(event, callback) { this.events[event] = callback; }
  querySelectorAll() { return [...this.children]; }
  appendChild(child) { this.children.push(child); child.parent = this; }
  removeChild(child) { this.children = this.children.filter(c => c !== child); }
  remove() { this.parent?.removeChild(this); }
  removeAttribute(name) { delete this[name]; }
  pause() { this.paused = true; }
  play() { this.paused = false; return Promise.resolve(); }
  load() { this.loads++; }
  click() {}
}

(async () => {
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, new Element(id));
    return elements.get(id);
  };
  const requests = [];
  const anchors = [];
  const context = vm.createContext({
    document: { getElementById: element, createElement: tag => { const el = new Element(tag); if (tag === 'a') anchors.push(el); return el; }, body: new Element('body') },
    AbortSignal: { timeout: () => undefined },
    fetch: async (url, options) => {
      requests.push({ url, method: options.method });
      return { ok: true, text: async () => '{}' };
    },
    setInterval: () => 1, clearInterval: () => {}, confirm: () => true,
    location: {}, history: {}, URLSearchParams, TextEncoder, console,
  });
  const html = fs.readFileSync(path.join(__dirname, '../templates/pkuvideo_fw.html'), 'utf8');
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1]
    .replace('checkLogin().then(() => openManualReplay());', '');
  vm.runInContext(script, context);
  vm.runInContext('activeJobId = "job-1";', context);
  element('fwPlayer').dataset.variant = 'source';
  element('subtitleVersion').value = 'zh';
  element('videoPreview').dataset.jobId = 'job-1';

  assert.match(html, /id="downloadTranslatedBtn"[^>]* hidden disabled/);
  for (const status of [null, 'queued', 'running', 'failed', 'needs_review', 'deleting']) {
    context.testTranslation = status ? {status, stage:'test'} : null;
    vm.runInContext('renderTranslation(testTranslation)', context);
    assert.equal(element('downloadTranslatedBtn').hidden, true, `Hide translated download for ${status}`);
  }
  vm.runInContext('renderTranslation({status:"running",stage:"中文配音",completed_blocks:2,total_blocks:4});', context);
  assert.equal(element('translateBtn').disabled, true);
  assert.equal(element('downloadTranslatedBtn').disabled, true);
  assert.match(element('translationStatus').textContent, /2\/4/);
  vm.runInContext('renderTranslation({status:"running",stage:"翻译与术语处理 已完成 8/16块",completed_blocks:0,total_blocks:16});',context);
  assert.match(element('translationStatus').textContent,/8\/16/);
  assert.doesNotMatch(element('translationStatus').textContent,/0\/16/,'Translation progress must not be confused with unfinished dubbing blocks');

  const ready = 'renderTranslation({status:"succeeded",stage:"翻译完成",subtitle_precision:"asr"});';
  vm.runInContext(ready, context);
  const player = element('fwPlayer');
  assert.equal(player.src, '/api/pkuvideo_fw/jobs/job-1/translation/video.mp4');
  player.events.loadedmetadata();
  assert.equal(player.currentTime, 42);
  assert.equal(player.playbackRate, 1.25);
  assert.equal(player.volume, 0.4);
  assert.equal(player.muted, true);
  assert.equal(player.paused, false);
  assert.equal(player.children[0].src, '/api/pkuvideo_fw/jobs/job-1/translation/subtitles/zh.dub.vtt');
  assert.equal(element('downloadTranslatedBtn').disabled, false);
  assert.equal(element('downloadTranslatedBtn').hidden, false);

  const loads = player.loads;
  vm.runInContext(ready, context);
  assert.equal(player.loads, loads, 'Polling must not reset playback');
  vm.runInContext('switchAudio("source");', context);
  player.events.loadedmetadata();
  assert.equal(player.src, '/api/pkuvideo_fw/jobs/job-1/video.mp4');
  assert.equal(player.children[0].src, '/api/pkuvideo_fw/jobs/job-1/translation/subtitles/zh.source.vtt');
  vm.runInContext(ready, context);
  assert.equal(player.dataset.variant, 'source', 'A later status poll must preserve the user selection');

  element('subtitleVersion').value = 'off';
  vm.runInContext('updateSubtitleTracks();', context);
  assert.equal(player.children.length, 0);
  await vm.runInContext('downloadVideo("job-1")', context);
  vm.runInContext('downloadTranslatedVideo()', context);
  assert.equal(anchors[0].href, '/api/pkuvideo_fw/jobs/job-1/video.mp4?download=1');
  assert.equal(anchors[1].href, '/api/pkuvideo_fw/jobs/job-1/translation/video.mp4?download=1');
  await vm.runInContext('deleteJob("job-1")', context);
  assert.equal(requests.at(-1).method, 'DELETE');
  assert.equal(vm.runInContext('activeJobId', context), '');
  assert.equal(element('videoPreview').dataset.jobId, undefined);
  assert.equal(vm.runInContext('translationReady', context), false);
  console.log('FW translation UI: progress, source switch, playback state, subtitles, dual download and deletion checks passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
