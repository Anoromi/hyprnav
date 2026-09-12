const { test } = require('node:test');
const assert = require('node:assert/strict');
const { workspaceUrl } = require('../browser-extension/url.js');
test('changes only the target, preserving all unrelated bytes', () => {
  assert.equal(workspaceUrl('https://app.test/path?x=a+b&workspace=old&x=%2f&&flag#part%20one', 'workspace', '日本 &?=#'),
    'https://app.test/path?x=a+b&workspace=%E6%97%A5%E6%9C%AC%20%26%3F%3D%23&x=%2f&&flag#part%20one');
});
test('adds missing parameter before the fragment', () => {
  assert.equal(workspaceUrl('http://localhost:3000/a#b', 'workspace', 'alpha'), 'http://localhost:3000/a?workspace=alpha#b');
  assert.equal(workspaceUrl('https://app.test/?keep=1&', 'workspace', 'alpha'), 'https://app.test/?keep=1&&workspace=alpha');
});
test('recognizes encoded names without rewriting them', () => {
  assert.equal(workspaceUrl('https://app.test/?work%73pace=old&other=%ZZ', 'workspace', 'new'), 'https://app.test/?work%73pace=new&other=%ZZ');
});
test('rejects ambiguous parameters and privileged URLs', () => {
  assert.throws(() => workspaceUrl('https://app.test/?workspace=a&workspace=b', 'workspace', 'c'));
  assert.throws(() => workspaceUrl('file:///etc/passwd', 'workspace', 'c'));
});
