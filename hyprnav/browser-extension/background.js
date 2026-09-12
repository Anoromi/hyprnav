/* global workspaceUrl */
if (typeof importScripts === 'function') importScripts('url.js');
const browser = globalThis.browser || globalThis.chrome;
const TAB_KEY = 'hyprnav-name';
const CONFIG_KEY = 'hyprnav-tabs';
let port;
let queue = Promise.resolve();

function validText(value, label) {
  if (typeof value !== 'string' || !value.trim() || value.length > 4096) {
    throw new Error(`${label} must be a nonempty string of at most 4096 characters`);
  }
  return value;
}
async function waitForUrl(id, url) {
  const expected = new URL(url).href;
  for (let attempt = 0; attempt < 120; attempt++) {
    const tab = await browser.tabs.get(id);
    if (tab.url === expected) return tab;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error('Tab did not navigate to the requested URL within six seconds');
}
async function tabName(id) {
  if (browser.sessions?.getTabValue) return browser.sessions.getTabValue(id, TAB_KEY);
  return (await browser.storage.session.get(`${TAB_KEY}:${id}`))[`${TAB_KEY}:${id}`];
}
async function nameTab(id, name) {
  if (browser.sessions?.setTabValue) return browser.sessions.setTabValue(id, TAB_KEY, name);
  await browser.storage.session.set({ [`${TAB_KEY}:${id}`]: name });
}
if (!browser.sessions?.setTabValue) {
  browser.tabs.onRemoved.addListener(id => {
    void browser.storage.session.remove(`${TAB_KEY}:${id}`);
  });
}
async function configurations() {
  return (await browser.storage.local.get(CONFIG_KEY))[CONFIG_KEY] || {};
}
async function locate(name, config) {
  const tabs = (await browser.tabs.query({})).filter(tab => !tab.incognito);
  const tagged = [];
  for (const tab of tabs) {
    if (await tabName(tab.id) === name) tagged.push(tab);
  }
  if (tagged.length > 1) throw new Error(`Multiple tabs are named ${name}; close the duplicate`);
  if (tagged.length) {
    // Firefox may briefly expose about:blank while a new tab starts loading.
    for (let attempt = 0; (!tagged[0].url || tagged[0].url === 'about:blank') && attempt < 60; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 50));
      tagged[0] = await browser.tabs.get(tagged[0].id);
    }
    if (new URL(tagged[0].url).origin !== new URL(config.url).origin) {
      throw new Error(`Tab ${name} navigated to another origin; return it to the app first`);
    }
    return tagged[0];
  }
  const matches = tabs.filter(tab => {
    try { return workspaceUrl(tab.url, config.param, '') === workspaceUrl(config.url, config.param, ''); }
    catch { return false; }
  });
  if (matches.length > 1) throw new Error(`Multiple matching tabs for ${name}; close the duplicates`);
  if (matches.length) {
    const otherName = await tabName(matches[0].id);
    if (otherName && otherName !== name) throw new Error(`Tab already belongs to ${otherName}`);
    await nameTab(matches[0].id, name);
    return matches[0];
  }
  return null;
}
async function handle(message) {
  const configs = await configurations();
  if (message.op === 'list') {
    const result = [];
    for (const [name, config] of Object.entries(configs)) {
      try {
        const tab = await locate(name, config);
        result.push({ name, param: config.param, tab_id: tab?.id ?? null, url: tab?.url ?? config.url });
      } catch (error) { result.push({ name, error: error.message }); }
    }
    return result;
  }
  const name = validText(message.name, 'Tab name');
  if (['__proto__', 'constructor', 'prototype'].includes(name)) throw new Error('Reserved tab name');
  if (message.op === 'open') {
    const url = validText(message.url, 'URL');
    const param = validText(message.param, 'Parameter');
    workspaceUrl(url, param, ''); // Validate before creating or saving anything.
    if (Object.hasOwn(configs, name) && (configs[name].url !== url || configs[name].param !== param)) {
      throw new Error(`Tab ${name} is already configured with a different URL or parameter`);
    }
    const config = { url, param };
    let tab = await locate(name, config);
    if (!tab) {
      tab = await browser.tabs.create({ url, active: true });
      await nameTab(tab.id, name);
    }
    configs[name] = config;
    await browser.storage.local.set({ [CONFIG_KEY]: configs });
    await browser.tabs.update(tab.id, { active: true });
    tab = await waitForUrl(tab.id, !tab.url || tab.url === 'about:blank' ? url : tab.url);
    await browser.windows.update(tab.windowId, { focused: true });
    return { name, tab_id: tab.id, url: tab.url };
  }
  if (message.op !== 'goto') throw new Error('Unknown browser operation');
  const value = validText(message.workspace, 'Workspace');
  if (!Object.hasOwn(configs, name)) throw new Error(`Unknown tab ${name}; use hyprnav tab open first`);
  const config = configs[name];
  let tab = await locate(name, config);
  const url = workspaceUrl(tab?.url || config.url, config.param, value);
  if (!tab) {
    tab = await browser.tabs.create({ url, active: true });
    await nameTab(tab.id, name);
  } else {
    await browser.tabs.update(tab.id, tab.url === url ? { active: true } : { url, active: true });
  }
  tab = await waitForUrl(tab.id, url);
  await browser.windows.update(tab.windowId, { focused: true });
  return { name, tab_id: tab.id, workspace: value, url: tab.url };
}
function connect() {
  if (port) return;
  const connection = browser.runtime.connectNative('hyprnav_browser');
  port = connection;
  connection.onMessage.addListener(message => {
    queue = queue.catch(() => {}).then(async () => {
      let reply;
      try { reply = { id: message.id, result: await handle(message) }; }
      catch (error) { reply = { id: message.id, error: error.message }; }
      if (port === connection) connection.postMessage(reply);
    });
  });
  connection.onDisconnect.addListener(() => {
    // Reading lastError acknowledges Chrome's native-host disconnect error.
    void browser.runtime.lastError;
    port = null;
    setTimeout(connect, 3000);
    if (browser.alarms) void browser.alarms.create('hyprnav-reconnect', { delayInMinutes: 1 });
  });
}
if (browser.alarms) browser.alarms.onAlarm.addListener(alarm => {
  if (alarm.name === 'hyprnav-reconnect') connect();
});
connect();
