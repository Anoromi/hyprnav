const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const crypto = require('node:crypto');
const path = require('node:path');
const root = path.join(__dirname, '../browser-extension');

for (const family of ['firefox', 'chromium']) {
  test(`${family}: reuse tabs, preserve URL bytes and recover after worker restart`, async () => {
    const local = {}, session = {}, tags = {}, tabs = new Map();
    let removed;
    const storage = data => ({
      get: async key => ({ [key]: data[key] }),
      set: async value => Object.assign(data, value),
      remove: async key => { delete data[key]; },
    });
    const api = {
      storage: { local: storage(local), session: storage(session) },
      runtime: { connectNative: () => ({ onMessage: { addListener() {} }, onDisconnect: { addListener() {} } }) },
      tabs: {
        query: async () => [...tabs.values()],
        get: async id => ({ ...tabs.get(id) }),
        create: async ({ url }) => {
          const tab = { id: 1, windowId: 2, url };
          tabs.set(tab.id, tab);
          // Chromium can initially return a pending tab without a URL.
          return family === 'chromium' ? { id: 1, windowId: 2 } : { ...tab };
        },
        update: async (id, props) => Object.assign(tabs.get(id), props),
        onRemoved: { addListener(fn) { removed = fn; } },
      },
      windows: { update: async () => {} },
    };
    if (family === 'firefox') api.sessions = {
      getTabValue: async id => tags[id],
      setTabValue: async (id, key, value) => { tags[id] = value; },
    };
    function worker() {
      const context = vm.createContext({ [family === 'firefox' ? 'browser' : 'chrome']: api, URL, setTimeout });
      vm.runInContext(fs.readFileSync(path.join(root, 'url.js'), 'utf8'), context);
      vm.runInContext(fs.readFileSync(path.join(root, 'background.js'), 'utf8'), context);
      return message => context.handle(message);
    }
    let handle = worker();
    const url = 'https://example.com/app?keep=a%20b&keep=2#section';
    const opened = await handle({ op: 'open', name: 'demo', url, param: 'workspace' });
    assert.equal(opened.url, url);
    const first = await handle({ op: 'goto', name: 'demo', workspace: 'research' });
    assert.equal(first.tab_id, opened.tab_id);
    assert.equal(first.url, 'https://example.com/app?keep=a%20b&keep=2&workspace=research#section');
    handle = worker();
    const next = await handle({ op: 'goto', name: 'demo', workspace: 'personal' });
    assert.equal(next.tab_id, opened.tab_id);
    assert.equal(tabs.size, 1);
    if (family === 'chromium') {
      tabs.delete(1);
      removed(1);
      assert.equal(session['hyprnav-name:1'], undefined);
      const reopened = await handle({ op: 'goto', name: 'demo', workspace: 'work' });
      assert.equal(reopened.workspace, 'work');
    }
  });
}
test('Chromium manifest key matches the native host extension ID', () => {
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'manifest.chromium.json')));
  const id = crypto.createHash('sha256').update(Buffer.from(manifest.key, 'base64')).digest('hex')
    .slice(0, 32).replace(/[0-9a-f]/g, digit => String.fromCharCode(97 + parseInt(digit, 16)));
  assert.equal(id, fs.readFileSync(path.join(root, 'chromium-id'), 'utf8').trim());
});
