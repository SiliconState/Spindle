// The page tools Spindle offers its threads in Chrome. Their definitions live here for
// the engine, which offers them; background.js does their work, since only it can
// reach tabs. Reading pages needs the user's say-so: the Page reading switch, and
// Chrome's access to sites (`PAGE_ACCESS`, which the switch asks for; the Linux's
// network uses it too). Acting needs the Page actions switch as well: Chrome's debugger
// (which Chrome does not offer as an optional permission, so the extension holds it and
// the switch decides) types and clicks as the user would, pictures a tab and watches
// its network. The switches are kept as `pages` ({reading, acting}) in the kv store.
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

const tab = { type: 'integer', description: 'A tab id from list_tabs (default: the tab the user is looking at)' };
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
      'scrolling down that many screens, each new line once.',
    inputSchema: {
      type: 'object',
      properties: {
        tab,
        max_chars: { type: 'integer', description: 'At most this many characters of page text (default 20000, at most 100000)' },
        scroll: { type: 'integer', minimum: 1, maximum: 30, description: 'Read while scrolling down this many screens (it moves the page, so page actions must be on)' },
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
      'another tab. `ref` reads only that part; `hidden` includes what is not shown.',
    inputSchema: {
      type: 'object',
      properties: {
        tab,
        ref: { ...ref, description: 'Read only this part of the page' },
        depth: { type: 'integer', description: 'How many levels of the outline to go (default 30)' },
        hidden: { type: 'boolean', description: 'Include parts that are not shown (default false)' },
        max_chars: { type: 'integer', description: 'At most this many characters (default 30000, at most 100000)' },
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

/** Acting: page actions on as well (Chrome's debugger). Not run again by themselves (a
 * click may not be safe to repeat), but for a picture. */
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
      'the page to load.',
    inputSchema: {
      type: 'object',
      properties: {
        url: { type: 'string', description: 'An http(s) address, or /workspace/NAME: a file of this thread opened in a tab for the user' },
        action: { type: 'string', enum: ['go', 'back', 'forward', 'reload'], description: 'Default: go (to url)' },
        tab,
        new_tab: { type: 'boolean', description: 'Open url in a new tab (default false)' },
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
    description: 'Choose options of a <select> list (by ref), by their value or text.',
    inputSchema: {
      type: 'object',
      properties: { ref, values: { type: 'array', items: { type: 'string' } }, tab },
      required: ['ref', 'values'],
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
export const ACTING_NAMES = new Set(ACTING.map((tool) => tool.name));
