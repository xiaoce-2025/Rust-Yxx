const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

(async () => {
  const html = fs.readFileSync(path.join(__dirname, '../templates/pkuvideo_sequence.html'), 'utf8');
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  new Function(script);
  assert.equal((html.match(/id="mimoModel"/g) || []).length, 1);
  assert.match(html, /type="password" id="mimoApiKey"/);
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, { value: '', placeholder: '', style: {}, events: {}, addEventListener(event, callback) { this.events[event] = callback; } });
    return elements.get(id);
  };
  const requests = [];
  const context = vm.createContext({
    document: { getElementById: element }, confirm: () => true, setTimeout: () => 1,
    api: async (url, method = 'GET', body) => {
      requests.push({ url, method, body });
      return { mimo_model: 'saved-tts', mimo_api_key_set: true };
    },
  });
  const section = script.slice(script.indexOf('async function loadApiConfig()'), script.indexOf('document.getElementById("keyConfirmBtn")'));
  vm.runInContext(section, context);
  await vm.runInContext('loadApiConfig()', context);
  assert.equal(element('mimoModel').value, 'saved-tts');
  assert.match(element('mimoApiKey').placeholder, /已设置/);
  assert.equal(element('mimoApiKey').value, '');

  element('mimoModel').value = ' custom-tts ';
  element('mimoApiKey').value = ' user-key ';
  await element('saveConfigBtn').events.click();
  assert.equal(requests.at(-1).url, '/api/user/api-config');
  assert.equal(requests.at(-1).body.mimo_model, 'custom-tts');
  assert.equal(requests.at(-1).body.mimo_api_key, 'user-key');
  assert.equal(element('mimoApiKey').value, '');
  element('mimoModel').value = '';
  await element('saveConfigBtn').events.click();
  assert.equal(requests.at(-1).body.mimo_api_key, null, 'Empty password must preserve the existing key');
  assert.equal(requests.at(-1).body.mimo_model, '', 'Empty model must restore the server default');
  await element('clearConfigBtn').events.click();
  assert.equal(requests.at(-1).body.mimo_model, '');
  assert.equal(requests.at(-1).body.mimo_api_key, '');
  assert.equal(element('mimoModel').value, '');
  assert.equal(element('mimoApiKey').placeholder, '输入后不可查看');
  console.log('MiMo API configuration UI: load, model/key save, preserve and clear passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
