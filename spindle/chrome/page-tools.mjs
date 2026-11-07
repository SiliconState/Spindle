// The page tools Spindle offers its threads in Chrome. Their definitions live here for
// the engine, which offers them; background.js does their work, since only it can
// reach tabs. Reading pages needs the user's say-so: the Page reading switch, and
// Chrome's access to sites (`PAGE_ACCESS`, which the switch asks for; the Linux's
// network uses it too). Acting needs the Page actions switch as well: Chrome's debugger
// (which Chrome does not offer as an optional permission, so the extension holds it and
// the switch decides) types and clicks as the user would, pictures a tab and watches
// its network. The switches are kept as `pages` ({reading, acting}) in the kv store.
/** The browser Spindle runs in, as what it says names it (its prompts, its debugging bar,
 * its access to sites are that browser's): Edge, Brave or Opera by their own name (Brave
 * gives it only in the brands it lists, not in its user agent), and Chrome for Chrome and
 * any other Chromium relative. */
const BRANDS = (globalThis.navigator?.userAgentData?.brands ?? []).map(({ brand }) => brand);
const AGENT = globalThis.navigator?.userAgent ?? '';
export const BROWSER = BRANDS.includes('Microsoft Edge') || /\bEdg(e|A|iOS)?\//.test(AGENT) ? 'Edge'
  : BRANDS.includes('Brave') ? 'Brave'
  : BRANDS.some((brand) => /^Opera\b/.test(brand)) || /\bOPR\//.test(AGENT) ? 'Opera'
  : 'Chrome';
export const PAGE_ACCESS = ['https://*/*', 'http://*/*'];
/** What Chrome asks of an extension that pictures the tab showing (sites alone do not
 * do): asked the first time the user takes one, and the tool uses it once given. */
export const PICTURE_ACCESS = ['<all_urls>'];

/** The switches as kept: each is on only when the user turned it on (Chrome's access to
 * sites alone, which a thread's network asks for too, turns on nothing). */
export function pageSwitches(kept, access) {
  const reading = access && kept?.reading === true;
  return { reading, acting: reading && kept?.acting === true };
}

const tab = { type: 'integer', description: 'A tab id from list_tabs (default: the tab the user is looking at, or a new tab you opened behind it while the user wrote in Spindle, until it is shown)' };
const ref = { type: 'string', description: 'A part of the page from page_elements, such as "kqm12" (each page names its own)' };

/** Reading: page reading on is enough. `rerun: safe` (reading twice changes nothing). */
const READING = [
  {
    name: 'list_tabs',
    description:
      'List the tabs open in Chrome: the id, title and address of each, and which one the user is looking at.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'read_page',
    description:
      'Read a web page open in Chrome: its address, title, the text the user selected on it, and its visible ' +
      'text. Reads the tab the user is looking at unless `tab` names another (an id from list_tabs). For a feed ' +
      'or list that loads as it scrolls (and drops what scrolled away), give `scroll`: one call reads while ' +
      'scrolling down that many screens, each new line once. With `find`, only the lines that match it come ' +
      'back (each with its neighbours), however long the page. A file of this workspace open in a Spindle tab ' +
      '(/workspace/NAME) reads as the file shows.',
    inputSchema: {
      type: 'object',
      properties: {
        tab,
        max_chars: { type: 'integer', description: 'At most this many characters of page text (default 20000, at most 100000)' },
        scroll: { type: 'integer', minimum: 1, maximum: 30, description: 'Read while scrolling down this many screens (it moves the page, so page actions must be on)' },
        find: { type: 'string', description: 'Only lines holding these words (case insensitive), or matching /pattern/flags' },
      },
      additionalProperties: false,
    },
    // 100000 characters of page text with its heading lines, in UTF-8.
    maxOutput: 512 * 1024,
  },
  {
    name: 'page_elements',
    description:
      'The page as its parts: headings, links, buttons, fields, lists, tables and text, indented as nested, each ' +
      'part one can act on with a ref (such as kqm12) for click, type, select and scroll. Refs last while the ' +
      'page stays, and name parts of that page only: read again after it changes, and pass `tab` when acting on ' +
      'another tab. `ref` reads only that part; `hidden` includes what is not shown; `find` keeps only the parts ' +
      'whose line matches it (a button by its words, say), each under the parts above it.',
    inputSchema: {
      type: 'object',
      properties: {
        tab,
        ref: { ...ref, description: 'Read only this part of the page' },
        depth: { type: 'integer', description: 'How many levels of the outline to go (default 30)' },
        hidden: { type: 'boolean', description: 'Include parts that are not shown (default false)' },
        max_chars: { type: 'integer', description: 'At most this many characters (default 30000, at most 100000)' },
        find: { type: 'string', description: 'Only parts whose line holds these words (case insensitive), or matches /pattern/flags' },
      },
      additionalProperties: false,
    },
    maxOutput: 512 * 1024,
  },
  {
    name: 'page_html',
    description:
      "A page's HTML (or one part's, by ref), its scripts left out unless `scripts` is true.",
    inputSchema: {
      type: 'object',
      properties: {
        tab,
        ref: { ...ref, description: 'Only this part of the page' },
        scripts: { type: 'boolean', description: 'Keep <script> elements (default false)' },
        max_chars: { type: 'integer', description: 'At most this many characters (default 50000, at most 200000)' },
      },
      additionalProperties: false,
    },
    maxOutput: 1024 * 1024,
  },
];

/** What one `select` with `files` may put in a field: per file, how many, and all of
 * them (they reach Chrome's side as base64 in one message, which Chrome caps at 64 MiB). */
export const UPLOAD = { file: 25 * 1024 * 1024, count: 10, total: 40 * 1024 * 1024 };

/** Acting: page actions on as well (Chrome's debugger). Not run again by themselves (a
 * click may not be safe to repeat), but for a picture and a wait. */
const ACTING = [
  {
    name: 'screenshot',
    description:
      'A picture of what a tab shows, saved in the workspace (screenshots/, JPEG): the tab showing in its ' +
      'window once the user let Spindle picture pages (the camera beside the message box asks), any tab with ' +
      'page actions on. The user sees it in the panel; you do not: with bash in the browser Linux, ' +
      '`spindle model --image FILE "QUESTION"` has a model look at it.',
    inputSchema: { type: 'object', properties: { tab }, additionalProperties: false },
    maxOutput: 8 * 1024,
    rerun: 'safe',
  },
  {
    name: 'navigate',
    description:
      'Go to an address in a tab showing a web page (or in a new tab), or back, forward or reload; waits for ' +
      'the page to load. Tabs you open (and tabs a page opens on your click) close once this turn is done.',
    inputSchema: {
      type: 'object',
      properties: {
        url: { type: 'string', description: 'An http(s) address, or /workspace/NAME: a file of this thread opened in a tab for the user' },
        action: { type: 'string', enum: ['go', 'back', 'forward', 'reload'], description: 'Default: go (to url)' },
        tab,
        new_tab: { type: 'boolean', description: 'Open url in a new tab (default false)' },
        keep_open: { type: 'boolean', description: 'With new_tab: leave the tab open after this turn (a page the user asked to have open)' },
      },
      additionalProperties: false,
    },
  },
  {
    name: 'click',
    description: 'Click a part of the page (by ref from page_elements), as the user would.',
    inputSchema: {
      type: 'object',
      properties: {
        ref,
        tab,
        double: { type: 'boolean', description: 'A double click' },
        button: { type: 'string', enum: ['left', 'right', 'middle'] },
      },
      required: ['ref'],
      additionalProperties: false,
    },
  },
  {
    name: 'type',
    description:
      'Type text into a field (by ref), as the user would: after what is there, or instead of it with `clear`; ' +
      '`submit` presses Enter after.',
    inputSchema: {
      type: 'object',
      properties: {
        ref,
        text: { type: 'string' },
        tab,
        clear: { type: 'boolean', description: 'Replace what the field holds (default false)' },
        submit: { type: 'boolean', description: 'Press Enter after (default false)' },
      },
      required: ['ref', 'text'],
      additionalProperties: false,
    },
  },
  {
    name: 'select',
    description:
      'Choose options of a <select> list (by ref), by their value or text; or, with `files`, put files of the ' +
      'workspace in a file field (by ref: the field, its label, or the button or drop area beside it), as if ' +
      'the user picked them in its dialog. Many sites upload a file the moment it is chosen, and some fill the ' +
      'form from it: put in only files the user meant to send, before typing the rest.',
    inputSchema: {
      type: 'object',
      properties: {
        ref,
        values: { type: 'array', items: { type: 'string' } },
        files: { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: UPLOAD.count, description: 'Workspace paths, such as resume.pdf' },
        tab,
      },
      required: ['ref'],
      additionalProperties: false,
    },
  },
  {
    name: 'press',
    description:
      'Press a key in a tab, such as Enter, Escape, Tab, ArrowDown, PageDown, Backspace, or a combination such as ' +
      'Control+a or Shift+Tab.',
    inputSchema: {
      type: 'object',
      properties: { key: { type: 'string' }, tab },
      required: ['key'],
      additionalProperties: false,
    },
  },
  {
    name: 'scroll',
    description: 'Scroll a part (by ref) into view, or the page up, down, to the top or to the bottom.',
    inputSchema: {
      type: 'object',
      properties: {
        ref,
        to: { type: 'string', enum: ['up', 'down', 'top', 'bottom'], description: 'Default: down (a screen)' },
        tab,
      },
      additionalProperties: false,
    },
  },
  {
    name: 'wait_for',
    description:
      'Wait until text (or a CSS selector) shows on the page, or is gone with `gone`, at most `seconds` (default ' +
      '10, at most 25).',
    inputSchema: {
      type: 'object',
      properties: {
        text: { type: 'string' },
        selector: { type: 'string' },
        gone: { type: 'boolean' },
        seconds: { type: 'number' },
        tab,
      },
      additionalProperties: false,
    },
    rerun: 'safe',
  },
  {
    name: 'dialog',
    description:
      'Answer the alert, confirm or prompt dialog a page opened (an action says when one opens): accept or ' +
      'dismiss it, with text for a prompt.',
    inputSchema: {
      type: 'object',
      properties: { accept: { type: 'boolean' }, text: { type: 'string' }, tab },
      required: ['accept'],
      additionalProperties: false,
    },
  },
  {
    name: 'close_tab',
    description: 'Close a tab showing a web page (by its id from list_tabs).',
    inputSchema: { type: 'object', properties: { tab }, required: ['tab'], additionalProperties: false },
  },
  {
    name: 'network',
    description:
      "Watch a tab's network: `start` records its requests from then on, `read` lists them (id, method, status, " +
      'type, size, address; `filter` keeps those whose address holds it; `body` gives one response by id), ' +
      '`stop` ends it.',
    inputSchema: {
      type: 'object',
      properties: {
        action: { type: 'string', enum: ['start', 'read', 'stop'] },
        tab,
        filter: { type: 'string' },
        body: { type: 'string', description: 'A request id from read: its response body' },
      },
      required: ['action'],
      additionalProperties: false,
    },
    maxOutput: 512 * 1024,
  },
];

export const PAGE_TOOLS = [
  ...READING.map((tool) => ({ ...tool, rerun: 'safe' })),
  ...ACTING.map((tool) => ({ rerun: 'no', ...tool })),
];

/** A page tool's input with its numbers as numbers: some models quote them ("tab":
 * "2054624637"), which Chrome refuses in words that do not say why, and a model told only
 * that tries the same again. A number field (by the tool's schema) given its number in
 * quotes gets the number (an integer field a whole one, a number field a decimal too);
 * anything else stays as given (Chrome's side says what is wrong). */
export function numbersIn(tool, input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) return input;
  let fixed = null;
  for (const [key, schema] of Object.entries(tool.inputSchema?.properties ?? {})) {
    const value = input[key];
    const whole = schema?.type === 'integer';
    if (typeof value !== 'string' || !(whole || schema?.type === 'number')) continue;
    if (!(whole ? /^\s*-?\d+\s*$/ : /^\s*-?\d+(\.\d+)?\s*$/).test(value)) continue;
    const number = Number(value);
    if (whole ? Number.isSafeInteger(number) : Number.isFinite(number)) (fixed ??= { ...input })[key] = number;
  }
  return fixed ?? input;
}

export const ACTING_NAMES = new Set(ACTING.map((tool) => tool.name));

/** A page tool's answer when what it did is not known (`why`: Chrome's answer never
 * came, or the tool failed once it had begun acting): one safe to repeat (a read, a
 * picture, a wait) just failed; an action may have reached the page, so whether it did
 * is unknown (the model checks before trying again, and a script makes no further calls). */
export function uncertain(name, why) {
  return PAGE_TOOLS.find((tool) => tool.name === name)?.rerun === 'safe' ? { error: why } : { unknown: true, content: why };
}

/** Settings, Pages, Before acting: `auto` (the default) asks before an action that
 * sends or submits something; `ask` before every action that changes a page or a tab;
 * `full` acts without asking. Observing (screenshots, scrolling, waiting, the network
 * log) never asks. An install from before Auto was the default keeps Full, the default
 * it had (background.js, as it updates). */
export const CONFIRM_MODES = ['full', 'auto', 'ask'];
export const ASKING_NAMES = new Set(['navigate', 'click', 'type', 'select', 'press', 'dialog', 'close_tab']);
/** A control's words that mean a click sends, commits or spends something. */
const SENDING_WORDS = /\b(send|submit|post|publish|reply|comment|tweet|share|buy|pay|order|checkout|check out|purchase|confirm|delete|remove|transfer|subscribe|donate|book|apply)\b/i;

/** The mode kept with the Pages switches (anything else reads as the default). */
export function confirmMode(kept) {
  return CONFIRM_MODES.includes(kept?.confirm) ? kept.confirm : 'auto';
}

/** The Pages record to write: `next` over the record as read (`kept`: undefined when
 * there is none, null when it could not be read). It always names Before acting, so no
 * record written now looks like one from before Auto was the default (which
 * background.js turns to Full as it updates); a choice that could not be read is
 * written as Ask, as the engine treats it. */
export function pagesRecord(kept, next) {
  return { ...(kept ?? {}), confirm: kept === null ? 'ask' : confirmMode(kept), ...next };
}

/** Whether `name` with `input` sends or submits something: Enter, typing with submit, a
 * page dialog accepted, files put in a field (many sites upload them at once), or a
 * click on a submit control or one whose words say it sends
 * (`target`: what the click lands on, from the page; null when that could not be told,
 * which counts as sending). */
export function sends(name, input, target) {
  if (name === 'type') return input?.submit === true;
  // "Return" is pressed as Enter (background.js ALIASES).
  if (name === 'press') return /(^|\+)\s*((numpad)?enter|return)$/i.test(String(input?.key ?? '').trim());
  if (name === 'dialog') return input?.accept === true;
  if (name === 'select') return Array.isArray(input?.files) && input.files.length > 0;
  // Its words, and its accessible name (an icon button labelled "Send").
  if (name === 'click') return !target || target.submits === true || SENDING_WORDS.test(target.label ?? '') || SENDING_WORDS.test(target.what ?? '');
  return false;
}

/** The action as the user is asked about it ("Click “Send” (button) on mail.example"). */
export function actionText(name, input, target) {
  // A page's or a model's quotation marks made plain (they cannot close the question's).
  const plain = (text) => String(text ?? '').replace(/[“”«»„]/g, '"');
  const quote = (text, most = 80) => {
    const clean = plain(text).replace(/\s+/g, ' ').trim();
    return `“${clean.length > most ? `${clean.slice(0, most - 1)}…` : clean}”`;
  };
  const what = plain(target?.what ?? (input?.ref ? `part ${input.ref}` : 'the page'));
  // What the action is held to besides its name (background.js asShown): its words, when
  // the name (cut short) does not hold them all, and where it leads.
  const words = String(target?.words ?? '').replace(/\s+/g, ' ').trim();
  const saying = words && !String(what).toLowerCase().includes(words.toLowerCase()) ? ` saying ${quote(words, 160)}` : '';
  const going = target?.dest ? `, going to ${target.dest}` : '';
  const where = target?.host ? ` on ${target.host}` : '';
  // The tab an action without a part lands on, named (its title, then its site).
  const inTab = target?.tab != null ? ` in tab ${target.tab}${target.title ? ` (${quote(target.title, 60)}${target.host ? `, ${target.host}` : ''})` : target.host ? ` (${target.host})` : ''}` : '';
  switch (name) {
    case 'click': return `${input?.double ? 'Double-click' : input?.button === 'right' ? 'Right-click' : 'Click'} ${what}${saying}${where}${going}`;
    case 'type': return `Type ${quote(input?.text, 300)} into ${what}${where}${input?.submit ? ', then press Enter' : ''}`;
    case 'select': {
      if (!Array.isArray(input?.files) || !input.files.length) return `Choose ${(input?.values ?? []).map((value) => quote(value, 40)).join(', ')} in ${what}${where}`;
      // Each file with its size, so what goes out is what the user agrees to; a name's
      // own quotation marks made plain, so it cannot pass for the rest of the question.
      const size = (bytes) => (bytes >= 1048576 ? `${(bytes / 1048576).toFixed(1)} MB` : bytes >= 1024 ? `${Math.round(bytes / 1024)} KB` : `${bytes} bytes`);
      // `upload`: the files as the engine read them (`engine-worker.mjs` uploadOf).
      const files = (input.upload ?? input.files.map((path) => ({ path }))).map((file) => `${quote(file.path, 80)}${file.size != null ? ` (${size(file.size)})` : ''}`);
      return `Upload ${files.join(', ')} to ${what}${saying}${where}`;
    }
    // The part Enter goes to, when one has the focus (another form may submit otherwise).
    case 'press': return `Press ${String(input?.key ?? '')}${target?.focus ? ` in ${plain(target.focus.what)}` : ''}${inTab || where}`;
    case 'navigate': {
      const action = input?.action ?? 'go';
      if (action === 'back') return `Go back a page${inTab}`;
      if (action === 'forward') return `Go forward a page${inTab}`;
      if (action === 'reload') return `Reload the page${inTab}`;
      return input?.new_tab ? `Open in a new tab ${String(input?.url ?? '')}` : `Go to ${String(input?.url ?? '')}${inTab}`;
    }
    // The tab first, then what the dialog says, whole and with its own quotation marks
    // made plain, so a page's words cannot pass for the tab or the end of the question.
    case 'dialog': {
      const says = target?.dialog?.message ? `, which says ${quote(target.dialog.message, 500)}` : '';
      return `${input?.accept ? 'Accept' : 'Dismiss'} the page's ${target?.dialog?.type ? `${target.dialog.type} ` : ''}dialog${inTab}${says}${input?.text ? `, answering ${quote(input.text)}` : ''}`;
    }
    case 'close_tab': return `Close tab ${input?.tab}${target?.title ? ` (${quote(target.title, 60)}${target.host ? `, ${target.host}` : ''})` : target?.host ? ` (${target.host})` : ''}`;
    default: return name;
  }
}
