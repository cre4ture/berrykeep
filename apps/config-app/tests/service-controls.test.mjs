import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const rust = readFileSync(new URL('../src/main.rs', import.meta.url), 'utf8');
const script = rust.split('const APP_JS: &str = r###"')[1].split('"###;')[0];

function app(statuses = []) {
  const elements = new Map();
  const fixture = {
    platform: 'windows', supports_os_integration: true,
    store: {
      client_identities: [], os_integration_instances: [], folder_agent_instances: [],
      client_cli_instances: [{ id: 'client-a', label: 'Client A', enabled: true }],
    },
    service_statuses: statuses,
  };
  const context = vm.createContext({
    fixture, URL,
    window: { addEventListener() {}, setTimeout() {} },
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, { value: '', dataset: {} });
        return elements.get(id);
      },
    },
  });
  vm.runInContext(script, context);
  vm.runInContext('renderConfig(fixture)', context);
  return { context, elements, fixture };
}

test('rapid Start clicks send one request and show pending feedback immediately', async () => {
  const { context, elements, fixture } = app();
  let calls = 0;
  let finish;
  const response = new Promise((resolve) => { finish = resolve; });
  context.fetch = async () => { calls++; return response; };
  const first = context.window.controlService('client-cli', 'client-a', 'start');
  const second = context.window.controlService('client-cli', 'client-a', 'start');
  assert.equal(calls, 1);
  assert.match(elements.get('client-instance-list').innerHTML, /Starting/);
  assert.match(elements.get('client-instance-list').innerHTML, /disabled>Start/);
  fixture.service_statuses = [{ instance_kind: 'client-cli', id: 'client-a', running: true, pid: 42, web_ui_ready: false }];
  finish({ ok: true, json: async () => ({ config: fixture }) });
  await Promise.all([first, second]);
  assert.match(elements.get('client-instance-list').innerHTML, /Starting/);
});

test('readiness polling updates cards without clearing unsaved form edits', async () => {
  const { context, elements } = app([{ instance_kind: 'client-cli', id: 'client-a', running: true, pid: 42, web_ui_ready: false }]);
  assert.match(elements.get('client-instance-list').innerHTML, /Starting/);
  assert.doesNotMatch(elements.get('client-instance-list').innerHTML, /href="http:\/\/127.0.0.1:8081/);
  context.document.getElementById('client-label').value = 'Unsaved label';
  context.fetch = async (url) => {
    assert.equal(url, '/api/service-statuses');
    return { ok: true, json: async () => [{ instance_kind: 'client-cli', id: 'client-a', running: true, pid: 42, web_ui_ready: true }] };
  };
  await vm.runInContext('refreshServiceStatuses()', context);
  assert.match(elements.get('client-instance-list').innerHTML, /Running/);
  assert.match(elements.get('client-instance-list').innerHTML, /href="http:\/\/127.0.0.1:8081/);
  assert.equal(elements.get('client-label').value, 'Unsaved label');
});

test('a failed action releases controls and shows the returned error', async () => {
  const { context, elements } = app();
  context.fetch = async () => ({ ok: false, json: async () => ({ error: 'Already starting', code: 'service_action_in_progress' }) });
  await context.window.controlService('client-cli', 'client-a', 'start');
  assert.match(elements.get('status-output').textContent, /Already starting/);
  assert.doesNotMatch(elements.get('client-instance-list').innerHTML, /disabled>Start/);
  assert.doesNotMatch(elements.get('client-instance-list').innerHTML, /Starting/);
});

test('an older status poll cannot overwrite a completed Start response', async () => {
  const { context, elements, fixture } = app();
  let finishPoll;
  const pollResponse = new Promise((resolve) => { finishPoll = resolve; });
  context.fetch = async (url) => {
    if (url === '/api/service-statuses') return pollResponse;
    fixture.service_statuses = [{ instance_kind: 'client-cli', id: 'client-a', running: true, pid: 42, web_ui_ready: true }];
    return { ok: true, json: async () => ({ config: fixture }) };
  };
  const poll = vm.runInContext('refreshServiceStatuses()', context);
  await context.window.controlService('client-cli', 'client-a', 'start');
  finishPoll({ ok: true, json: async () => [] });
  await poll;
  assert.match(elements.get('client-instance-list').innerHTML, /Running/);
});

test('a process that exits during startup is no longer shown as Starting', async () => {
  const { context, elements } = app([{ instance_kind: 'client-cli', id: 'client-a', running: true, web_ui_ready: false }]);
  context.fetch = async () => ({ ok: true, json: async () => [{ instance_kind: 'client-cli', id: 'client-a', running: false }] });
  await vm.runInContext('refreshServiceStatuses()', context);
  assert.match(elements.get('client-instance-list').innerHTML, /Stopped/);
  assert.doesNotMatch(elements.get('client-instance-list').innerHTML, /Starting/);
  assert.doesNotMatch(elements.get('client-instance-list').innerHTML, /disabled>Start/);
});
