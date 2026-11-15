const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const script = fs.readFileSync(path.join(__dirname, '../../userscripts/pku-replay-link.user.js'), 'utf8');

function environment(fixtures) {
  const requests = [], copied = [];
  const elements = {
    '.pku-copy-btn': {disabled: false},
    '.pku-copy-status': {dataset: {}, style: {}}
  };
  class XHR {
    open() {}
    send() {}
    addEventListener(name, handler) { this[name] = handler; }
  }
  const context = vm.createContext({
    XMLHttpRequest: XHR, URL, Uint8Array, Set,
    location: {href: 'https://onlineroomse.pku.edu.cn/player?course_id=1'},
    document: {readyState: 'loading', addEventListener() {}, querySelector: s => elements[s]},
    setTimeout() {}, clearTimeout() {}, navigator: {},
    GM_setClipboard(text) { copied.push(text); },
    GM_xmlhttpRequest(options) {
      requests.push(options);
      const fixture = fixtures[options.url];
      assert.ok(fixture, 'Unexpected request: ' + options.url);
      if (typeof fixture === 'function') return fixture(options);
      options.onload({status: 200, finalUrl: options.url, ...fixture});
    }
  });
  vm.runInContext(script.replace('    function init() {', '    globalThis.testApi = {linkWithKey, handleCopyLink};\n    function init() {'), context);
  function video(url) {
    const xhr = new XHR();
    xhr.open('GET', '/get-sub-info-by-auth-data');
    xhr.send();
    xhr.responseText = JSON.stringify({list:[{sub_content: JSON.stringify({save_playback:{contents:url}})}]});
    xhr.load();
  }
  return {api: context.testApi, requests, copied, elements, video};
}
const media = 'https://resourcese.pku.edu.cn/video.m3u8';
const keyUrl = 'https://resourcese.pku.edu.cn/enc.key';
const manifest = '#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="enc.key"\n#EXTINF:10,\nsegment.ts';
const bytes = Uint8Array.from({length:16}, (_, i) => 240 + i);

test('copies binary key as encoded hex and preserves signed query parameters', async () => {
  const url = media + '?token=a%20b%2Fc&v=1';
  const env = environment({[url]:{responseText:manifest}, [keyUrl]:{response:bytes.buffer}});
  env.video(url);
  await env.api.handleCopyLink();
  assert.equal(env.copied[0], url + '&enc_key=hex%3Af0f1f2f3f4f5f6f7f8f9fafbfcfdfeff');
  assert.equal(env.requests[1].responseType, 'arraybuffer');
  assert.equal(env.requests[1].anonymous, false);
  assert.equal(env.elements['.pku-copy-btn'].disabled, false);
});

test('follows a master playlist and resolves key relative to redirected media URL', async () => {
  const env = environment({
    [media]:{responseText:'#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nsub/playlist.m3u8'},
    'https://resourcese.pku.edu.cn/sub/playlist.m3u8':{finalUrl:'https://resourcese.pku.edu.cn/final/video.m3u8',responseText:manifest},
    'https://resourcese.pku.edu.cn/final/enc.key':{response:bytes.buffer}
  });
  const link = await env.api.linkWithKey(media);
  assert.equal(new URL(link).searchParams.get('enc_key'), 'hex:f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff');
  assert.ok(link.startsWith(media + '?enc_key='));
});

test('refreshes an existing enc_key parameter without duplicating it', async () => {
  const env = environment({[media]:{responseText:manifest}, [keyUrl]:{response:bytes.buffer}});
  const link = await env.api.linkWithKey(media + '?enc_key=old#fragment');
  assert.equal(new URL(link).searchParams.getAll('enc_key').length, 1);
  assert.ok(!link.includes('#fragment'));
});

test('failed key retrieval never copies an incomplete link', async () => {
  for (const response of [{response:new ArrayBuffer(5)}, {status:403}, {response:new ArrayBuffer(0)}]) {
    const env = environment({[media]:{responseText:manifest}, [keyUrl]:response});
    env.video(media);
    await env.api.handleCopyLink();
    assert.equal(env.copied.length, 0);
    assert.equal(env.elements['.pku-copy-status'].dataset.error, 'true');
    assert.equal(env.elements['.pku-copy-btn'].disabled, false);
  }
});

test('HTML login response and multiple encryption keys produce explicit errors', async () => {
  let env = environment({[media]:{responseText:'<html>Login</html>'}});
  await assert.rejects(env.api.linkWithKey(media), /不是 m3u8/);
  env = environment({[media]:{responseText:manifest + '\n#EXT-X-KEY:METHOD=AES-128,URI="other.key"'}});
  await assert.rejects(env.api.linkWithKey(media), /多个密钥/);
});

test('switching lessons while retrieving the key prevents copying a stale link', async () => {
  let finish;
  const env = environment({[media]:{responseText:manifest}, [keyUrl]: options => { finish = () => options.onload({status:200,response:bytes.buffer}); }});
  env.video(media);
  const pending = env.api.handleCopyLink();
  await new Promise(resolve => setImmediate(resolve));
  env.video(media + '?lesson=2');
  finish();
  await pending;
  assert.equal(env.copied.length, 0);
  assert.match(env.elements['.pku-copy-status'].textContent, /已切换回放/);
});

test('MP4 and unencrypted manifests do not request a key', async () => {
  const env = environment({[media]:{responseText:'#EXTM3U\n#EXTINF:10,\nvideo.ts'}});
  assert.equal(await env.api.linkWithKey('https://resourcese.pku.edu.cn/video.mp4'), 'https://resourcese.pku.edu.cn/video.mp4');
  assert.equal(await env.api.linkWithKey(media), media);
  assert.equal(env.requests.length, 1);
});
