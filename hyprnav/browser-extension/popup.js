const browser = globalThis.browser || globalThis.chrome;
const toggle = document.getElementById('primary');
const title = name => name ? name[0].toUpperCase() + name.slice(1) : 'none';

async function ask(message) {
  const reply = await browser.runtime.sendMessage(message);
  if (reply.error) throw new Error(reply.error);
  return reply.result;
}
function show(status) {
  toggle.checked = status.primary === status.browser;
  toggle.disabled = false;
  document.getElementById('current').textContent = `Primary: ${title(status.primary)}`;
  document.getElementById('connected').textContent =
    `Connected: ${status.connected.map(title).join(', ') || 'none'}`;
  document.getElementById('error').textContent = '';
}
function fail(error) {
  document.getElementById('error').textContent = error.message;
}
toggle.addEventListener('change', () => {
  toggle.disabled = true;
  ask({ op: 'set_primary', primary: toggle.checked }).then(show, fail);
});
ask({ op: 'status' }).then(show, fail);
