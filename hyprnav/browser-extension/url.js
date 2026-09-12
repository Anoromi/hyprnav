/* Preserve unrelated URL bytes, including query ordering, +, escapes and #. */
function workspaceUrl(raw, parameter, value) {
  const parsed = new URL(raw);
  if (!['http:', 'https:'].includes(parsed.protocol)) throw new Error('Only HTTP(S) tabs are supported');
  if (!parameter || typeof parameter !== 'string') throw new Error('Parameter name must not be empty');
  const hashAt = raw.indexOf('#');
  const hash = hashAt < 0 ? '' : raw.slice(hashAt);
  const main = hashAt < 0 ? raw : raw.slice(0, hashAt);
  const queryAt = main.indexOf('?');
  const base = queryAt < 0 ? main : main.slice(0, queryAt);
  const query = queryAt < 0 ? '' : main.slice(queryAt + 1);
  let matches = 0;
  const parts = query === '' ? [] : query.split('&').map(part => {
    const equals = part.indexOf('=');
    const key = equals < 0 ? part : part.slice(0, equals);
    let decoded;
    try { decoded = decodeURIComponent(key.replace(/\+/g, ' ')); } catch { return part; }
    if (decoded !== parameter) return part;
    matches++;
    return `${key}=${encodeURIComponent(value)}`;
  });
  if (matches > 1) throw new Error(`URL has multiple ${parameter} parameters; choose a URL with one`);
  if (!matches) parts.push(`${encodeURIComponent(parameter)}=${encodeURIComponent(value)}`);
  return `${base}?${parts.join('&')}${hash}`;
}
if (typeof module !== 'undefined') module.exports = { workspaceUrl };
