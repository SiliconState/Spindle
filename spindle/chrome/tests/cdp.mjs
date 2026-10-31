// Real Chrome driven over the DevTools protocol on a pipe (--remote-debugging-pipe), as
// Spindle for Browser's tests drive it. Extensions load through Extensions.loadUnpacked
// (--enable-unsafe-extension-debugging): branded Chrome ignores --load-extension since
// 137. Clicks and typing are real input events, so the page sees user gestures.
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';

/** Starts Chrome on `profile` (`args`: more of its flags, such as a net log). */
export async function launch(binary, { profile, downloads, headless = true, args = [] }) {
  const child = spawn(binary, [
    ...(headless ? ['--headless=new'] : []),
    '--no-first-run', '--no-default-browser-check', '--disable-gpu', '--window-size=1200,900',
    '--remote-debugging-pipe', '--enable-unsafe-extension-debugging', `--user-data-dir=${profile}`, ...args, 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
  const [write, read] = [child.stdio[3], child.stdio[4]];
  const waiting = new Map();
  const listeners = new Set();
  let next = 0;
  let buffer = '';
  let exited = null;
  child.stderr.resume();
  child.on('exit', (code, signal) => {
    exited = `Chrome exited (${signal ?? code})`;
    for (const { reject } of waiting.values()) reject(new Error(exited));
    waiting.clear();
  });
  read.on('data', (chunk) => {
    buffer += chunk;
    for (let end = buffer.indexOf('\0'); end >= 0; end = buffer.indexOf('\0')) {
      const message = JSON.parse(buffer.slice(0, end));
      buffer = buffer.slice(end + 1);
      if (message.id != null && waiting.has(message.id)) {
        const { resolve, reject, method } = waiting.get(message.id);
        waiting.delete(message.id);
        if (message.error) reject(new Error(`${method}: ${message.error.message}`));
        else resolve(message.result);
      } else if (message.method) {
        for (const listener of listeners) listener(message);
      }
    }
  });
  write.on('error', () => {});
  const send = (method, params = {}, sessionId) => {
    if (exited) return Promise.reject(new Error(exited));
    const id = ++next;
    return new Promise((resolve, reject) => {
      waiting.set(id, { resolve, reject, method });
      write.write(`${JSON.stringify({ id, method, params, ...(sessionId && { sessionId }) })}\0`);
    });
  };
  const browser = {
    child,
    send,
    on: (listener) => listeners.add(listener),
    off: (listener) => listeners.delete(listener),
    /** Ends Chrome as a crash would: no clean shutdown. */
    crash() {
      child.kill('SIGKILL');
      return new Promise((done) => (exited ? done() : child.once('exit', done)));
    },
    async close() {
      if (exited) return;
      const ended = new Promise((done) => child.once('exit', done));
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      try {
        await send('Browser.close').catch(() => {});
        await ended;
      } finally {
        clearTimeout(timer);
      }
    },
  };
  if (process.env.SPINDLE_CONSOLE) {
    // Every context's console and uncaught errors (pages, the offscreen document, their
    // workers), for finding where something stalls.
    const attach = (sessionId) => {
      send('Runtime.enable', {}, sessionId).catch(() => {});
      send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, sessionId).catch(() => {});
    };
    browser.on(({ method, params, sessionId }) => {
      if (method === 'Target.attachedToTarget') attach(params.sessionId);
      else if (method === 'Runtime.consoleAPICalled') {
        console.log(`[console ${params.type}] ${params.args.map((arg) => arg.value ?? arg.description ?? '').join(' ')}`);
      } else if (method === 'Runtime.exceptionThrown') {
        console.log(`[exception] ${params.exceptionDetails.exception?.description ?? params.exceptionDetails.text}`);
      }
    });
    await send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true });
    await send('Target.setDiscoverTargets', { discover: true });
  }
  if (downloads) {
    // Each download lands as DIR/<guid>; Chrome says when it is complete.
    await send('Browser.setDownloadBehavior', { behavior: 'allowAndName', downloadPath: downloads, eventsEnabled: true });
    const names = new Map();
    browser.nextDownload = (timeout = 30_000) => new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        browser.off(watch);
        reject(new Error('no download completed'));
      }, timeout);
      const watch = ({ method, params }) => {
        if (method === 'Browser.downloadWillBegin') names.set(params.guid, params.suggestedFilename);
        if (method === 'Browser.downloadProgress' && params.state === 'completed') {
          clearTimeout(timer);
          browser.off(watch);
          resolve({ path: `${downloads}/${params.guid}`, name: names.get(params.guid) });
        }
      };
      browser.on(watch);
    });
  }
  return browser;
}

/** Opens `url` in a new tab (or window) and returns a handle to drive it. */
export async function open(browser, url, { newWindow = false } = {}) {
  const { targetId } = await browser.send('Target.createTarget', { url, newWindow });
  const { sessionId } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
  const send = (method, params) => browser.send(method, params, sessionId);
  await send('Runtime.enable');
  const page = {
    targetId,
    sessionId,
    async eval(expression) {
      const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
      if (result.exceptionDetails) {
        throw new Error(`in the page: ${result.exceptionDetails.exception?.description ?? result.exceptionDetails.text}`);
      }
      return result.result.value;
    },
    /** Waits until `expression` is truthy; returns its value. */
    async until(expression, { timeout = 30_000, what = expression } = {}) {
      const end = Date.now() + timeout;
      let last;
      for (;;) {
        try {
          last = await page.eval(expression);
          if (last) return last;
        } catch (error) {
          last = error.message;
        }
        if (Date.now() > end) throw new Error(`timed out waiting for ${what} (last: ${JSON.stringify(last)})`);
        await new Promise((done) => setTimeout(done, 200));
      }
    },
    async click(selector) {
      const where = () => page.until(`(() => {
        const node = document.querySelector(${JSON.stringify(selector)});
        if (!node || node.disabled || node.hidden || !node.offsetParent && getComputedStyle(node).position !== 'fixed') return null;
        node.scrollIntoView({ block: 'center' });
        const r = node.getBoundingClientRect();
        return r.width && r.height ? { x: r.x + r.width / 2, y: r.y + r.height / 2 } : null;
      })()`, { what: `${selector} to be clickable` });
      // Where it stays: content still coming in can move it between looking and clicking.
      let box = await where();
      for (let tries = 0; tries < 20; tries++) {
        await new Promise((done) => setTimeout(done, 150));
        const again = await where();
        if (again.x === box.x && again.y === box.y) break;
        box = again;
      }
      for (const type of ['mousePressed', 'mouseReleased']) {
        await send('Input.dispatchMouseEvent', { type, x: box.x, y: box.y, button: 'left', clickCount: 1 });
      }
    },
    async type(selector, text) {
      await page.until(`(() => { const node = document.querySelector(${JSON.stringify(selector)}); if (!node) return false; node.focus(); return document.activeElement === node; })()`, { what: `${selector} to take focus` });
      await send('Input.insertText', { text });
    },
    async files(selector, paths) {
      const { root } = await send('DOM.getDocument', { depth: 0 });
      const { nodeId } = await send('DOM.querySelector', { nodeId: root.nodeId, selector });
      await send('DOM.setFileInputFiles', { nodeId, files: paths });
    },
    async screenshot(path) {
      const { data } = await send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(path, Buffer.from(data, 'base64'));
    },
    /** Lays the page out at this size (a side panel's, say). */
    size: (width, height) => send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false }),
    /** Lays the page out at its window's size again. */
    unsize: () => send('Emulation.clearDeviceMetricsOverride'),
    activate: () => browser.send('Target.activateTarget', { targetId }),
    close: () => browser.send('Target.closeTarget', { targetId }),
  };
  return page;
}
