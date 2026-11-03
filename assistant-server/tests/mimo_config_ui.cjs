const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

(async () => {
  const template = name => fs.readFileSync(path.join(__dirname, '../templates', name), 'utf8');
  const sequence = template('pkuvideo_sequence.html');
  new vm.Script(sequence.match(/<script>([\s\S]*?)<\/script>/)[1]);
  assert.doesNotMatch(sequence, /toggleConfigBtn|\/api\/user\/api-config/);
  const html = template('user-center.html');
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  const control = () => ({ value: '', events: {}, addEventListener(event, fn) { this.events[event] = fn; } });
  const providers = ['aliyun', 'deepseek', 'mimo'];
  const models = providers.map(provider => ({ ...control(), name: provider + '_model' }));
  const buttons = providers.map(provider => ({ ...control(), dataset: { key: provider + '_api_key' } }));
  const mode = Object.assign([control(), control()], { value: '' });
  const form = {
    ...control(),
    elements: { ai_mode: mode },
    querySelectorAll: selector => selector === '[data-key]' ? buttons : models,
  };
  for (const model of models) {
    assert.ok(html.includes('type="text" name="' + model.name + '"'));
    form.elements[model.name] = model;
  }
  for (const { dataset: { key } } of buttons) {
    assert.ok(html.includes('type="password" name="' + key + '"'));
    form.elements[key] = control();
  }
  const status = { removeAttribute() {}, setAttribute() {} };
  const elements = { 'api-form': form, 'api-fields': {}, 'api-status': status, 'custom-settings': {} };
  const config = { ai_mode: 'custom', mimo_model: 'saved-tts', mimo_api_key_set: true };
  const requests = [];
  const context = vm.createContext({
    window: { siteAccount: { then() {} } },
    document: { getElementById: id => elements[id] },
    fetch: async (url, options) => {
      assert.equal(url, '/api/user/api-config');
      if (options.method === 'POST') {
        const body = JSON.parse(options.body);
        requests.push(body);
        Object.assign(config, body);
        if ('mimo_api_key' in body) config.mimo_api_key_set = Boolean(body.mimo_api_key);
      }
      return { ok: true, json: async () => config };
    },
  });
  await vm.runInContext(script, context);
  const key = form.elements.mimo_api_key;
  assert.equal(form.elements.mimo_model.value, 'saved-tts');
  assert.match(key.placeholder, /已保存/);
  assert.equal(key.value, '');
  models.forEach(model => { model.value = ' custom-' + model.name + ' '; });
  key.value = ' user-key ';
  const submit = () => form.events.submit({ preventDefault() {} });
  await submit();
  for (const model of models) assert.equal(requests.at(-1)[model.name], 'custom-' + model.name);
  assert.equal(requests.at(-1).mimo_api_key, 'user-key');
  assert.equal(key.value, '');
  form.elements.mimo_model.value = '';
  await submit();
  assert.equal(requests.at(-1).mimo_api_key, undefined);
  assert.equal(requests.at(-1).mimo_model, '');
  buttons[2].events.click();
  await submit();
  assert.equal(requests.at(-1).mimo_api_key, '');
  assert.equal(key.placeholder, '未配置');
  mode.value = 'server';
  await submit();
  assert.deepEqual(requests.at(-1), { ai_mode: 'server' });
})().catch(error => { console.error(error); process.exitCode = 1; });
