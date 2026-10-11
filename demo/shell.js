// Management-only host simulator. Message envelope and isolation follow the
// pinned codex-proxy-rs bridge v2; production UI/resources are loaded unchanged.
const frame = document.querySelector('iframe');
const button = document.querySelector('#theme');
const envelope = { target: 'codex-proxy-plugin-management', version: 2, channel: 'audit-demo', session: `${Date.now()}-${Math.random()}` };
const matches = (message) => message && Object.entries(envelope).every(([key, value]) => message[key] === value);

function installBridge(initial) {
  let sequence = 0, theme = initial.theme.name;
  const pending = new Map();
  const matches = (message) => message && ['target', 'version', 'channel', 'session'].every((key) => message[key] === initial[key]);
  function applyTheme(value) {
    theme = value.name;
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    for (const [key, token] of Object.entries(value.tokens)) document.documentElement.style.setProperty(key, token);
    window.dispatchEvent(new CustomEvent('codex-proxy-themechange', { detail: { theme } }));
  }
  function layout(height) { document.documentElement.style.setProperty('--cp-plugin-viewport-height', `${height}px`); }
  window.addEventListener('message', (event) => {
    if (event.source !== parent || event.origin !== initial.parentOrigin || !matches(event.data)) return;
    const message = event.data;
    if (message.type === 'theme') { applyTheme(message.payload); return; }
    if (message.type === 'layout') { layout(message.payload.viewportHeight); return; }
    if (message.type !== 'result') return;
    const operation = pending.get(message.id);
    if (!operation) return;
    clearTimeout(operation.timer); pending.delete(message.id);
    if (message.ok) operation.resolve(message.payload);
    else operation.reject(new Error(message.error));
  });
  const api = Object.freeze({ version: 2, plugin: Object.freeze(initial.plugin), page: Object.freeze(initial.page),
    get theme() { return theme; },
    request(input) {
      if (pending.size >= 8) return Promise.reject(new Error('Too many management requests'));
      return new Promise((resolve, reject) => {
        const id = ++sequence;
        const timer = setTimeout(() => { pending.delete(id); reject(new Error('Management request timed out')); }, 30000);
        pending.set(id, { resolve, reject, timer });
        parent.postMessage({ target: initial.target, version: initial.version, channel: initial.channel, session: initial.session, type: 'request', id, payload: input }, initial.parentOrigin);
      });
    },
    resourceUrl(path) {
      if (!Object.hasOwn(initial.resourceUrls, path)) throw new Error('Unregistered resource');
      return initial.resourceUrls[path];
    },
  });
  Object.defineProperty(window, 'codexProxyPlugin', { value: api, configurable: false, writable: false });
  applyTheme(initial.theme); layout(initial.viewportHeight);
}

async function start() {
  const response = await fetch('/plugin.json');
  if (!response.ok) throw new Error('无法加载插件资源');
  const { registration, resources, themes } = await response.json();
  const resourceUrls = Object.fromEntries(Object.entries(resources).map(([path, resource]) => [path, `data:${resource.contentType};base64,${resource.body}`]));
  const page = registration.pages[0];
  const send = (type, payload) => frame.contentWindow.postMessage({ ...envelope, type, payload }, '*');
  let theme = 'light';
  const applyTheme = () => {
    document.documentElement.dataset.theme = theme;
    for (const [key, value] of Object.entries(themes[theme].tokens)) document.documentElement.style.setProperty(key, value);
    button.textContent = theme === 'light' ? '切换深色主题' : '切换浅色主题';
    send('theme', themes[theme]);
  };
  button.addEventListener('click', () => { theme = theme === 'light' ? 'dark' : 'light'; applyTheme(); });
  applyTheme();
  window.addEventListener('message', async (event) => {
    if (event.source !== frame.contentWindow || !matches(event.data) || event.data.type !== 'request') return;
    const { id, payload } = event.data;
    try {
      if (!registration.routes.some((route) => route.method === payload.method && route.path === payload.path)
        || (payload.body !== undefined && (!(payload.body instanceof ArrayBuffer) || payload.contentType !== 'application/json')) || typeof payload.query !== 'string' || payload.query.startsWith('?') || payload.query.includes('#')
        || new TextEncoder().encode(payload.query).byteLength > 8192) throw new Error('Invalid management request');
      const result = await fetch(`/__demo/${payload.path}?${payload.query}`, { method: payload.method, ...(payload.body ? { headers: { 'content-type': payload.contentType }, body: payload.body } : {}) });
      const body = await result.arrayBuffer();
      if (body.byteLength > 1024 * 1024) throw new Error('Management response exceeds host limit');
      frame.contentWindow.postMessage({ ...envelope, type: 'result', id, ok: true, payload: {
        status: result.status, contentType: result.headers.get('content-type'), body,
      } }, '*', [body]);
    } catch (error) {
      frame.contentWindow.postMessage({ ...envelope, type: 'result', id, ok: false, error: error.message }, '*');
    }
  });
  const decoder = new TextDecoder();
  let html = decoder.decode(Uint8Array.from(atob(resources[page.entry].body), (letter) => letter.charCodeAt(0)));
  html = html.replace(/(src|href)="\.\/([^"?#]+)"/g, (_, attribute, path) => {
    if (!resourceUrls[path]) throw new Error('Unregistered page resource');
    return `${attribute}="${resourceUrls[path]}"`;
  });
  const initial = { ...envelope, parentOrigin: location.origin, plugin: { name: 'prompt-audit' }, page: { id: page.id, title: page.title }, theme: themes[theme], viewportHeight: frame.clientHeight, resourceUrls };
  const csp = "default-src 'none'; script-src 'unsafe-inline' data:; style-src 'unsafe-inline' data:; img-src data:; font-src data:; media-src data:; connect-src 'none'; worker-src 'none'; child-src 'none'; frame-src 'none'; object-src 'none'; manifest-src 'none'; base-uri 'none'; form-action 'none'";
  html = html.replace('<head>', `<head><meta http-equiv="Content-Security-Policy" content="${csp}"><style>html{overflow:hidden}body{min-height:var(--cp-plugin-viewport-height,0px)}</style><script>(${installBridge.toString()})(${JSON.stringify(initial).replace(/</g, '\\u003c')})</script>`);
  frame.srcdoc = html;
  new ResizeObserver(() => send('layout', { viewportHeight: frame.clientHeight })).observe(frame);
}
start().catch((error) => {
  document.querySelector('#error').hidden = false;
  document.querySelector('#error').textContent = error.message;
});
