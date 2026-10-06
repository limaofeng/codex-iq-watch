const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function setup() {
  const nodes = new Map();
  const calls = [];
  let send;
  const document = {
    activeElement: null,
    querySelectorAll: () => [],
    addEventListener() {},
    getElementById(id) {
      if (!nodes.has(id)) {
        const events = {};
        const node = {
          value: '', hidden: true, open: false, disabled: false, textContent: '', innerHTML: '', isConnected: true,
          querySelectorAll: () => [],
          addEventListener: (name, fn) => { events[name] = fn; },
          emit: (name, event = {}) => events[name]?.(event),
          focus: () => { document.activeElement = node; },
          showModal: () => { node.open = true; },
          close: () => { node.open = false; events.close?.({}); },
        };
        nodes.set(id, node);
      }
      return nodes.get(id);
    },
  };
  const reply = (body, status = 200) => ({status, body: new TextEncoder().encode(JSON.stringify(body)).buffer});
  const window = {setTimeout() {}, clearTimeout() {}, codexProxyPlugin: {request: async input => {
    calls.push(input);
    if (input.method === 'GET') return reply({accounts: [], alerts: [], accounts_error: '账号不可读', storage_warning: '状态已满'});
    return new Promise((resolve, reject) => { send = {resolve: body => resolve(reply(body)), reject}; });
  }}};
  const context = vm.createContext({window, document, TextDecoder, ArrayBuffer, console});
  let source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
  source = source.replace(/\}\)\(\);\s*$/, 'window.test = {clearAccount};})();');
  vm.runInContext(source, context);
  return {window, document, node: id => document.getElementById(id), calls, send: () => send};
}
async function flush() { for (let i=0;i<20;i++) await Promise.resolve(); }

for (const action of ['clear', 'reset']) {
  test(`${action}: 取消不写入，确认防双击，失败可见且可重试`, async () => {
    const h = setup(); await flush();
    const open = () => action === 'clear' ? h.window.test.clearAccount('account') : h.node('reset-settings').emit('click');
    const origin = h.node('origin'); origin.focus();
    open(); assert.equal(h.node('confirm-dialog').open, true);
    assert.equal(h.document.activeElement, h.node('confirm-cancel'));
    h.node('confirm-cancel').emit('click');
    assert.equal(h.document.activeElement, origin);
    assert.equal(h.calls.filter(x => x.method === 'POST').length, 0);
    open(); const first = h.node('confirm-accept').emit('click');
    h.node('confirm-accept').emit('click');
    assert.equal(h.calls.filter(x => x.method === 'POST').length, 1);
    let prevented = false;
    h.node('confirm-dialog').emit('cancel', {preventDefault() { prevented = true; }});
    assert.equal(prevented, true);
    h.send().reject(new Error('synthetic failure')); await first;
    assert.match(h.node('confirm-error').textContent, /synthetic failure/);
    assert.equal(h.node('confirm-dialog').open, true);
    const second = h.node('confirm-accept').emit('click');
    h.send().resolve({ok: true}); await second;
    assert.equal(h.node('confirm-dialog').open, false);
    assert.equal(h.calls.filter(x => x.method === 'POST').length, 2);
  });
}
test('账号错误与容量告警同时展示', async () => {
  const h = setup(); await flush();
  assert.match(h.node('status-sub').textContent, /账号不可读/);
  assert.match(h.node('status-sub').textContent, /状态已满/);
});


test('隔离页面通过按钮保存题目，不依赖浏览器表单提交', async () => {
  const h = setup(); await flush();
  h.node('question-new').emit('click');
  h.node('question-title').value = '测试题';
  h.node('question-prompt').value = '只输出最终答案';
  h.node('question-answers').value = '21\n22';
  const saving = h.node('question-save').emit('click', {preventDefault() {}});
  const sent = h.calls.find(call => call.path === 'question-save');
  assert.equal(sent.method, 'POST');
  const input = JSON.parse(sent.body);
  assert.equal(input.expected_version, null);
  assert.equal(input.question.title, '测试题');
  assert.deepEqual(input.question.answers, ['21', '22']);
  assert.equal(h.node('question-save').disabled, true);
  h.send().resolve({questions:[{...input.question,id:'new',version:1}],version:1});
  await saving;
  assert.equal(h.node('question-form').hidden, true);
  assert.equal(h.node('questions-hint').textContent, '已保存');
});
