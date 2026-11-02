// Office documents (Word .docx, Excel .xlsx, PowerPoint .pptx, and their macro-enabled
// kinds, whose macros never run) drawn as plain pages for previews. They are read here,
// from the file's own parts (nothing is sent anywhere), into static HTML that page.mjs
// shows in the sandboxed preview frame, where nothing runs. Their text, headings, lists,
// tables, pictures, sheets and slides' layout come through; their fonts and spacing only
// roughly; shapes only as their text on a plain fill; charts and diagrams as a mark
// (said in `notes`); headers, footers, comments and speaker notes not at all: the
// download is the document.
import { base64 } from './page.mjs';

/** The largest document drawn (its download takes any size): callers check a file's
 * size against it before reading the file. */
export const MAX_DOCUMENT = 128 * 1024 * 1024;
/** Word's own bound on a list's start (and so on its numbers). */
const MAX_START = 32767;
/** One part unpacked at most (a small file can unpack to a lot). */
const MAX_PART = 64 * 1024 * 1024;
/** An XML part read at most: it is parsed whole, and its tree takes many times its size
 * in the page's memory. */
const MAX_XML = 16 * 1024 * 1024;
/** All parts read, together, at most. */
const MAX_UNPACKED = 256 * 1024 * 1024;
/** Pictures put in a page, together, each use counted (each use repeats its bytes). */
const MAX_PICTURES = 48 * 1024 * 1024;
/** A sheet's rows and columns shown, and its cells at most. */
const MAX_ROWS = 2000;
const MAX_COLUMNS = 100;
const MAX_CELLS = 100_000;
const MAX_SLIDES = 500;

/** `map[key]` only for a key `map` holds itself (a file's `constructor` names nothing). */
const own = (map, key) => (key != null && Object.hasOwn(map, key) ? map[key] : undefined);

const PICTURE = {
  png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', jpe: 'image/jpeg', gif: 'image/gif', bmp: 'image/bmp',
  webp: 'image/webp', svg: 'image/svg+xml', avif: 'image/avif', ico: 'image/x-icon',
};

/** The document `name` (`bytes`) as a whole static page: `{html, notes}`, `notes` saying
 * what it leaves out. Throws, saying why, for a file it cannot read. */
export async function documentPage(name, bytes) {
  if (bytes.length > MAX_DOCUMENT) throw new Error(`it is ${Math.round(bytes.length / 1048576)} MB: documents up to ${MAX_DOCUMENT / 1048576} MB show here`);
  const kind = /\.(doc|xls|ppt)[xm]$/i.exec(name)?.[1].toLowerCase();
  if (!kind) throw new Error('it is not a Word, Excel or PowerPoint document');
  const zip = unzip(bytes);
  const root = await relsOf(zip, '');
  const main = relation(root, 'officeDocument');
  if (!main) throw new Error('it holds no document (an Office file names its main part)');
  const notes = new Set();
  const pictures = picturesOf(zip, notes);
  const body = kind === 'doc' ? await word(zip, main, pictures, notes)
    : kind === 'xls' ? await workbook(zip, main, notes)
    : await deck(zip, main, pictures, notes);
  const core = await optional(zip, relation(root, 'core-properties'));
  const title = descendants(core?.documentElement, 'title')[0]?.textContent.trim() || name.split('/').pop();
  return {
    html: `<!doctype html>\n<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>${esc(title)}</title><style>${CSS[kind]}</style></head><body>${body}</body></html>`,
    notes: [...notes],
  };
}

// ---- Parts --------------------------------------------------------------------------

/** The parts of a zip (`bytes`): `part(name)` gives one's bytes (null: none such),
 * `text(name)` its text; within MAX_PART each and MAX_UNPACKED together. */
function unzip(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let end = -1;
  for (let at = bytes.length - 22; at >= Math.max(0, bytes.length - 22 - 0xffff); at--) {
    if (view.getUint32(at, true) === 0x06054b50) {
      end = at;
      break;
    }
  }
  if (end < 0) throw new Error('it is not a zip file, as an Office document is');
  const count = view.getUint16(end + 10, true);
  let at = view.getUint32(end + 16, true);
  const entries = new Map();
  for (let n = 0; n < count; n++) {
    if (at + 46 > bytes.length || view.getUint32(at, true) !== 0x02014b50) throw new Error('its zip directory is damaged');
    const length = view.getUint16(at + 28, true);
    const name = new TextDecoder().decode(bytes.subarray(at + 46, at + 46 + length));
    // Part names are told apart without case (Open Packaging Conventions).
    entries.set(name.toLowerCase(), {
      method: view.getUint16(at + 10, true),
      packed: view.getUint32(at + 20, true),
      size: view.getUint32(at + 24, true),
      local: view.getUint32(at + 42, true),
    });
    at += 46 + length + view.getUint16(at + 30, true) + view.getUint16(at + 32, true);
  }
  let unpacked = 0;
  const part = async (name, most = MAX_PART) => {
    const entry = name == null ? null : entries.get(name.toLowerCase());
    if (!entry) return null;
    if (entry.size === 0xffffffff || entry.size > most || unpacked + entry.size > MAX_UNPACKED) {
      throw Object.assign(new Error('it is too large for a preview: download it to open it'), { kind: 'too_large' });
    }
    if (entry.local + 30 > bytes.length || view.getUint32(entry.local, true) !== 0x04034b50) throw new Error('a part of it is damaged');
    const start = entry.local + 30 + view.getUint16(entry.local + 26, true) + view.getUint16(entry.local + 28, true);
    const packed = bytes.subarray(start, start + entry.packed);
    if (packed.length < entry.packed) throw new Error('it is cut short');
    unpacked += entry.size;
    if (entry.method === 0) return packed.subarray(0, entry.size);
    if (entry.method === 8) return inflate(packed, entry.size);
    throw new Error(`a part of it is packed in a way a preview does not read (method ${entry.method})`);
  };
  return {
    part,
    /** A part's size unpacked, as the directory says (null: none such). */
    size: (name) => (name == null ? null : entries.get(name.toLowerCase())?.size ?? null),
    text: async (name, most) => {
      const found = await part(name, most);
      if (!found) return found;
      // UTF-8, or UTF-16 by its byte order mark (both are allowed for a part).
      const encoding = found[0] === 0xff && found[1] === 0xfe ? 'utf-16le' : found[0] === 0xfe && found[1] === 0xff ? 'utf-16be' : 'utf-8';
      return new TextDecoder(encoding).decode(found);
    },
  };
}

/** `packed` (raw deflate) unpacked, at most `size` bytes (as its directory says). */
async function inflate(packed, size) {
  const reader = new Blob([packed]).stream().pipeThrough(new DecompressionStream('deflate-raw')).getReader();
  const out = new Uint8Array(size);
  let at = 0;
  for (;;) {
    const { done, value } = await reader.read().catch(() => {
      throw new Error('a part of it is damaged (it does not unpack)');
    });
    if (done) break;
    if (at + value.length > size) {
      await reader.cancel().catch(() => {});
      throw new Error('a part of it unpacks larger than its directory says');
    }
    out.set(value, at);
    at += value.length;
  }
  return at === size ? out : out.subarray(0, at);
}

/** A part as XML (null when there is no such part). */
async function xmlPart(zip, name) {
  const text = await zip.text(name, MAX_XML);
  if (text == null) return null;
  const doc = new DOMParser().parseFromString(text, 'application/xml');
  if (doc.getElementsByTagName('parsererror').length) throw new Error(`its part ${name} is not well-formed`);
  return doc;
}

/** A part that only adds to a document (its styles, numbering, notes, theme): null when
 * it is missing or damaged, so the rest still shows. */
const optional = (zip, name) => xmlPart(zip, name).catch(() => null);

/** A part's relationships (`''`: the package's own): id -> {target, external, type}. */
async function relsOf(zip, part) {
  const at = part.lastIndexOf('/') + 1;
  const dir = part.slice(0, at);
  const doc = await xmlPart(zip, `${dir}_rels/${part.slice(at)}.rels`);
  const rels = new Map();
  for (const rel of children(doc?.documentElement, 'Relationship')) {
    const target = rel.getAttribute('Target') ?? '';
    const external = rel.getAttribute('TargetMode') === 'External';
    rels.set(rel.getAttribute('Id'), { target: external ? target : partName(dir, target), external, type: rel.getAttribute('Type') ?? '' });
  }
  return rels;
}

/** The part a relationship of type `kind` (its last word) names, if any. */
const relation = (rels, kind) => [...rels.values()].find((rel) => !rel.external && rel.type.endsWith(`/${kind}`))?.target ?? null;

/** A part's name from `target`, relative to the part's folder `dir` (or absolute). */
function partName(dir, target) {
  let path = target;
  try {
    path = decodeURIComponent(target);
  } catch {}
  const out = [];
  for (const piece of (path.startsWith('/') ? path : dir + path).split('/')) {
    if (piece === '..') out.pop();
    else if (piece && piece !== '.') out.push(piece);
  }
  return out.join('/');
}

/** Pictures by part name as data: addresses, within MAX_PICTURES together (null: left
 * out, said in `notes`). */
function picturesOf(zip, notes) {
  let left = MAX_PICTURES;
  const made = new Map();
  return async (name) => {
    const type = own(PICTURE, name.split('.').pop().toLowerCase());
    if (!type) {
      notes.add('Some pictures are in a format a preview does not draw (such as EMF or WMF).');
      return null;
    }
    const size = zip.size(name);
    if (size == null) return null;
    // Each use counts: the page repeats the picture's bytes wherever it shows.
    if (size > left) {
      notes.add('Some pictures are left out: together they are too large for a preview.');
      return null;
    }
    if (!made.has(name)) made.set(name, zip.part(name).then((bytes) => (bytes ? `data:${type};base64,${base64(bytes)}` : null)).catch(() => null));
    const url = await made.get(name);
    if (!url) {
      notes.add('Some pictures could not be read.');
      return null;
    }
    left -= size;
    return url;
  };
}

// ---- XML ----------------------------------------------------------------------------

const children = (node, local = null) => [...(node?.children ?? [])].filter((element) => local == null || element.localName === local);
const child = (node, local) => children(node, local)[0] ?? null;
const descendants = (node, local) => (node ? [...node.getElementsByTagNameNS('*', local)] : []);
/** An attribute by its local name (any namespace). */
function attr(node, local) {
  for (const attribute of node?.attributes ?? []) if (attribute.localName === local) return attribute.value;
  return null;
}
/** A relationship id attribute (`r:id`, `r:embed`). */
function relAttr(node, local) {
  for (const attribute of node?.attributes ?? []) {
    if (attribute.localName === local && /relationships/.test(attribute.namespaceURI ?? '')) return attribute.value;
  }
  return null;
}
const val = (node) => attr(node, 'val');
/** An on/off property: there and not turned off. */
const on = (node) => node != null && !['0', 'false', 'off', 'none'].includes((val(node) ?? 'true').toLowerCase());
/** Alternate content: the first choice (the newest markup), else the fallback. */
const choice = (node) => child(node, 'Choice') ?? child(node, 'Fallback');

const esc = (text) => String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const hexColor = (value) => (/^[0-9a-f]{6}$/i.test(value ?? '') ? `#${value.toLowerCase()}` : null);
const px = (emu) => Math.round(Number(emu) / 9525);

// ---- Word ---------------------------------------------------------------------------

async function word(zip, main, pictures, notes) {
  const doc = await xmlPart(zip, main);
  if (!doc) throw new Error('its document part is missing');
  const rels = await relsOf(zip, main);
  const styles = wordStyles(await optional(zip, relation(rels, 'styles')));
  const ctx = {
    rels, pictures, notes, styles,
    numbering: wordNumbering(await optional(zip, relation(rels, 'numbering')), styles),
    counters: new Map(),
    referred: [],
  };
  const body = await blocks(child(doc.documentElement, 'body'), ctx);
  // Footnotes and endnotes, numbered as the text refers to them, after it.
  let after = '';
  if (ctx.referred.length) {
    const parts = {};
    for (const kind of ['footnotes', 'endnotes']) {
      const name = relation(rels, kind);
      parts[kind] = name ? { doc: await optional(zip, name), rels: await relsOf(zip, name).catch(() => new Map()) } : null;
    }
    // Footnotes first, then endnotes, each in the order the text refers to them.
    for (const { kind, id, label } of [...ctx.referred].sort((a, b) => (a.kind === b.kind ? 0 : a.kind === 'footnote' ? -1 : 1))) {
      const part = parts[kind === 'footnote' ? 'footnotes' : 'endnotes'];
      const note = children(part?.doc?.documentElement, kind).find((element) => attr(element, 'id') === id);
      if (!note) continue;
      const text = await blocks(note, { ...ctx, rels: part.rels, referred: [] });
      after += `<div class="note"><sup>${label}</sup>${text}</div>`;
    }
    if (after) after = `<section class="notes">${after}</section>`;
  }
  return `<article class="page">${body || '<p class="missing">This document has no text.</p>'}${after}</article>`;
}

/** Paragraph and character styles: headings, titles, lists and run formatting, as each
 * style (or one it is based on) says. */
function wordStyles(doc) {
  const byId = new Map();
  for (const style of children(doc?.documentElement, 'style')) {
    if (attr(style, 'styleId') == null) continue;
    byId.set(attr(style, 'styleId'), {
      id: attr(style, 'styleId') ?? '',
      name: (val(child(style, 'name')) ?? '').toLowerCase(),
      basedOn: val(child(style, 'basedOn')),
      pPr: child(style, 'pPr'),
      rPr: child(style, 'rPr'),
    });
  }
  const chain = (id) => {
    const found = [];
    for (let style = byId.get(id); style && found.length < 16 && !found.includes(style); style = byId.get(style.basedOn)) found.push(style);
    return found;
  };
  const paragraphs = new Map();
  const characters = new Map();
  const runs = new Map();
  // The document's run defaults, and the paragraph style a paragraph without one has.
  const defaults = runFlags(child(child(child(doc?.documentElement, 'docDefaults'), 'rPrDefault'), 'rPr'));
  const usual = children(doc?.documentElement, 'style')
    .find((style) => attr(style, 'type') === 'paragraph' && ['1', 'true', 'on'].includes(attr(style, 'default') ?? ''));
  return {
    /** What a paragraph style (its own, else the usual one) and the defaults give its
     * runs: hidden, bold, a color... (each run's own say comes after). */
    runsOf(id) {
      const key = id ?? attr(usual, 'styleId');
      if (!runs.has(key)) {
        const props = { ...defaults };
        for (const style of chain(key).reverse()) Object.assign(props, runFlags(style.rPr));
        runs.set(key, props);
      }
      return runs.get(key);
    },
    paragraph(id) {
      if (!paragraphs.has(id)) {
        const styles = chain(id);
        let level = null;
        for (const style of styles) {
          const heading = /^heading\s*([1-9])$/.exec(style.name) ?? /^heading([1-9])$/i.exec(style.id);
          const outline = val(child(style.pPr, 'outlineLvl'));
          if (heading) level = Number(heading[1]);
          else if (/^[0-8]$/.test(outline ?? '')) level = Number(outline) + 1;
          if (level) break;
        }
        const title = styles.some((style) => style.name === 'title');
        const numPr = styles.map((style) => child(style.pPr, 'numPr')).find(Boolean);
        paragraphs.set(id, {
          level: level ?? (title ? 1 : null),
          title,
          subtitle: styles.some((style) => style.name === 'subtitle'),
          numId: val(child(numPr, 'numId')),
          ilvl: val(child(numPr, 'ilvl')),
          align: styles.map((style) => val(child(style.pPr, 'jc'))).find(Boolean) ?? null,
        });
      }
      return paragraphs.get(id);
    },
    /** The list a paragraph style numbers its paragraphs with (a list style's own). */
    numIdOf(id) {
      return val(child(child(byId.get(id)?.pPr, 'numPr'), 'numId'));
    },
    character(id) {
      if (!characters.has(id)) {
        const props = {};
        for (const style of chain(id).reverse()) Object.assign(props, runFlags(style.rPr));
        characters.set(id, props);
      }
      return characters.get(id);
    },
  };
}

/** Word's highlight colors. */
const HIGHLIGHT = {
  yellow: '#ffff00', green: '#00ff00', cyan: '#00ffff', magenta: '#ff00ff', blue: '#0000ff', red: '#ff0000',
  darkBlue: '#000080', darkCyan: '#008080', darkGreen: '#008000', darkMagenta: '#800080', darkRed: '#800000',
  darkYellow: '#808000', darkGray: '#808080', lightGray: '#c0c0c0', black: '#000000', white: '#ffffff',
};

/** The formatting a run's properties set (only what they say: absent stays unset). */
function runFlags(rPr) {
  const props = {};
  const flag = (local, key) => {
    const node = child(rPr, local);
    if (node) props[key] = on(node);
  };
  flag('b', 'bold');
  flag('i', 'italic');
  const strikes = [child(rPr, 'strike'), child(rPr, 'dstrike')].filter(Boolean);
  if (strikes.length) props.strike = strikes.some(on);
  flag('caps', 'caps');
  flag('smallCaps', 'smallCaps');
  flag('vanish', 'hidden');
  const underline = child(rPr, 'u');
  if (underline) props.underline = on(underline);
  const position = val(child(rPr, 'vertAlign'));
  if (position) props.position = position;
  const color = hexColor(val(child(rPr, 'color')));
  if (color) props.color = color;
  const highlight = own(HIGHLIGHT, val(child(rPr, 'highlight')));
  if (highlight) props.highlight = highlight;
  const shade = hexColor(attr(child(rPr, 'shd'), 'fill'));
  if (shade && !props.highlight) props.highlight = shade;
  return props;
}

/** Lists: each numbering's levels (format, text such as `%1.`, start); a numbering that
 * links to a list style takes the levels that style's own numbering has. */
function wordNumbering(doc, styles) {
  const root = doc?.documentElement;
  const level = (lvl) => ({
    format: val(child(lvl, 'numFmt')) ?? 'decimal',
    text: val(child(lvl, 'lvlText')) ?? '',
    start: Math.min(MAX_START, Math.max(0, Number(val(child(lvl, 'start')) ?? 1) || 0)),
  });
  const abstract = new Map();
  for (const node of children(root, 'abstractNum')) {
    const levels = [];
    // Levels 0 to 8, as Word has (an index read from the file never sizes an array).
    for (const lvl of children(node, 'lvl')) {
      const at = Number(attr(lvl, 'ilvl')) || 0;
      if (Number.isInteger(at) && at >= 0 && at <= 8) levels[at] = level(lvl);
    }
    abstract.set(attr(node, 'abstractNumId'), { levels, link: val(child(node, 'numStyleLink')) });
  }
  const abstractOf = new Map(children(root, 'num').map((num) => [attr(num, 'numId'), val(child(num, 'abstractNumId'))]));
  const levelsOf = (id, depth = 0) => {
    const found = abstract.get(id);
    if (!found) return [];
    if (found.levels.length || !found.link || depth > 4) return found.levels;
    const numId = styles.numIdOf(found.link);
    return numId != null ? levelsOf(abstractOf.get(numId), depth + 1) : found.levels;
  };
  const lists = new Map();
  for (const num of children(root, 'num')) {
    const levels = [...levelsOf(val(child(num, 'abstractNumId')))];
    for (const override of children(num, 'lvlOverride')) {
      const at = Number(attr(override, 'ilvl')) || 0;
      if (!Number.isInteger(at) || at < 0 || at > 8) continue;
      const lvl = child(override, 'lvl');
      if (lvl) levels[at] = level(lvl);
      const start = val(child(override, 'startOverride'));
      if (start != null && levels[at]) levels[at] = { ...levels[at], start: Math.min(MAX_START, Math.max(0, Number(start) || 0)) };
    }
    // Numberings of one list continue each other's numbers (as Word counts them),
    // unless one overrides its levels or its start.
    const key = children(num, 'lvlOverride').length ? `num ${attr(num, 'numId')}` : `list ${val(child(num, 'abstractNumId'))}`;
    lists.set(attr(num, 'numId'), { levels, key });
  }
  return lists;
}

/** Block content (paragraphs and tables) of a body, cell, note or text box. */
async function blocks(node, ctx) {
  let html = '';
  for (const element of children(node)) {
    switch (element.localName) {
      case 'p': html += await paragraph(element, ctx); break;
      case 'tbl': html += await wordTable(element, ctx); break;
      case 'sdt': html += await blocks(child(element, 'sdtContent'), ctx); break;
      case 'customXml': case 'ins': case 'moveTo': case 'smartTag': html += await blocks(element, ctx); break;
      case 'AlternateContent': html += await blocks(choice(element), ctx); break;
      default: break;
    }
  }
  return html;
}

async function paragraph(p, ctx) {
  const pPr = child(p, 'pPr');
  const style = ctx.styles.paragraph(val(child(pPr, 'pStyle')));
  const outline = val(child(pPr, 'outlineLvl'));
  const level = /^[0-8]$/.test(outline ?? '') ? Number(outline) + 1 : style.level;
  const numPr = child(pPr, 'numPr');
  const numId = val(child(numPr, 'numId')) ?? style.numId;
  const ilvl = Math.min(8, Number(val(child(numPr, 'ilvl')) ?? style.ilvl ?? 0) || 0);
  const align = own({ center: 'center', right: 'right', end: 'right', both: 'justify', distribute: 'justify' }, val(child(pPr, 'jc')) ?? style.align);
  const css = align ? `text-align:${align}` : '';
  const listed = numId != null && numId !== '0';
  const mark = listed ? listMark(ctx, numId, ilvl) : '';
  const base = ctx.styles.runsOf(val(child(pPr, 'pStyle')));
  const text = await inline(p, { ...ctx, base });
  // A paragraph its style hides is not there at all.
  if (!text && base.hidden) return '';
  if (level) {
    const h = Math.min(level, 6);
    return `<h${h}${style.title ? ' class="title"' : ''}${css ? ` style="${css}"` : ''}>${mark ? `${esc(mark)} ` : ''}${text || '<br>'}</h${h}>`;
  }
  if (listed) {
    return `<p class="li" style="margin-left:${(1.6 * (ilvl + 1)).toFixed(1)}em${css ? `;${css}` : ''}"><span class="mark">${esc(mark)}</span>${text || '<br>'}</p>`;
  }
  return `<p${style.subtitle ? ' class="subtitle"' : ''}${css ? ` style="${css}"` : ''}>${text || '<br>'}</p>`;
}

/** A list item's mark: its bullet, or its number as its level writes it (`%1.%2`). */
function listMark(ctx, numId, ilvl) {
  const list = ctx.numbering.get(numId);
  const levels = list?.levels;
  const level = levels?.[ilvl];
  if (!level) return '•';
  if (level.format === 'bullet') return bullet(level.text);
  if (level.format === 'none') return '';
  const counts = ctx.counters.get(list.key) ?? [];
  counts[ilvl] = counts[ilvl] == null ? level.start : Math.min(MAX_START, counts[ilvl] + 1);
  // A deeper level starts again after this one.
  counts.length = ilvl + 1;
  ctx.counters.set(list.key, counts);
  return level.text.replace(/%([1-9])/g, (_, n) => {
    const at = Number(n) - 1;
    return counted(counts[at] ?? levels[at]?.start ?? 1, levels[at]?.format ?? 'decimal');
  });
}

/** A bullet as written, or what a symbol font's private character stands for. */
function bullet(text) {
  const symbols = { '\uf0b7': '•', '\uf0a7': '▪', '\uf0d8': '➢', '\uf0fc': '✓', '\uf076': '❖', '\uf0a8': '◆', '\uf06e': '■', o: '◦' };
  if (!text) return '•';
  if (own(symbols, text)) return own(symbols, text);
  return /[\ue000-\uf8ff]/.test(text) ? '•' : text;
}

/** `n` as a list numbers it. */
function counted(n, format) {
  const letters = (k) => (k < 1 ? String(k) : String.fromCharCode(97 + ((k - 1) % 26)).repeat(Math.floor((k - 1) / 26) + 1));
  const roman = (k) => {
    if (k < 1 || k > 3999) return String(k);
    let out = '';
    for (const [value, numeral] of [[1000, 'm'], [900, 'cm'], [500, 'd'], [400, 'cd'], [100, 'c'], [90, 'xc'], [50, 'l'], [40, 'xl'], [10, 'x'], [9, 'ix'], [5, 'v'], [4, 'iv'], [1, 'i']]) {
      for (; k >= value; k -= value) out += numeral;
    }
    return out;
  };
  switch (format) {
    case 'lowerLetter': return letters(n);
    case 'upperLetter': return letters(n).toUpperCase();
    case 'lowerRoman': return roman(n);
    case 'upperRoman': return roman(n).toUpperCase();
    case 'decimalZero': return n < 10 && n >= 0 ? `0${n}` : String(n);
    default: return String(n);
  }
}

/** A paragraph's (or hyperlink's, or field's) runs as inline HTML. */
async function inline(node, ctx) {
  let html = '';
  for (const element of children(node)) {
    switch (element.localName) {
      case 'r': html += await run(element, ctx); break;
      case 'hyperlink': {
        const rel = ctx.rels.get(relAttr(element, 'id'));
        const anchor = attr(element, 'anchor');
        const to = rel?.external ? rel.target : anchor ? `#${anchor}` : '';
        const text = await inline(element, ctx);
        // Shown as a link, its address on hover; a preview follows none.
        html += to ? `<span class="link" title="${esc(to)}">${text}</span>` : text;
        break;
      }
      case 'ins': case 'moveTo': case 'smartTag': case 'customXml': case 'fldSimple': case 'bdo': case 'dir':
        html += await inline(element, ctx);
        break;
      case 'sdt': html += await inline(child(element, 'sdtContent'), ctx); break;
      case 'AlternateContent': html += await inline(choice(element), ctx); break;
      case 'oMath': case 'oMathPara':
        html += `<span class="math">${esc(descendants(element, 't').map((t) => t.textContent).join(''))}</span>`;
        break;
      default: break;
    }
  }
  return html;
}

async function run(r, ctx) {
  const rPr = child(r, 'rPr');
  const props = { ...ctx.base, ...ctx.styles.character(val(child(rPr, 'rStyle'))), ...runFlags(rPr) };
  if (props.hidden) return '';
  let html = await runParts(r, ctx);
  if (!html) return '';
  const style = [
    props.color && `color:${props.color}`,
    props.highlight && `background:${props.highlight}`,
    props.caps && 'text-transform:uppercase',
    props.smallCaps && 'font-variant:small-caps',
  ].filter(Boolean).join(';');
  if (style) html = `<span style="${style}">${html}</span>`;
  if (props.position === 'superscript') html = `<sup>${html}</sup>`;
  else if (props.position === 'subscript') html = `<sub>${html}</sub>`;
  if (props.strike) html = `<s>${html}</s>`;
  if (props.underline) html = `<u>${html}</u>`;
  if (props.italic) html = `<i>${html}</i>`;
  if (props.bold) html = `<b>${html}</b>`;
  return html;
}

/** What a run holds: text, tabs, breaks, pictures, note marks. */
async function runParts(node, ctx) {
  let html = '';
  for (const element of children(node)) {
    switch (element.localName) {
      case 't': html += esc(element.textContent); break;
      case 'tab': case 'ptab': html += '<span class="tab"></span>'; break;
      case 'br': case 'cr': html += '<br>'; break;
      case 'noBreakHyphen': html += '\u2011'; break;
      case 'softHyphen': html += '\u00ad'; break;
      case 'sym': {
        const code = parseInt(attr(element, 'char') ?? '', 16);
        if (Number.isFinite(code) && code >= 0x20 && !(code >= 0xe000 && code <= 0xf8ff)) html += esc(String.fromCodePoint(code));
        break;
      }
      case 'drawing': html += await drawing(element, ctx); break;
      case 'pict': case 'object': html += await vml(element, ctx); break;
      case 'AlternateContent': html += await runParts(choice(element), ctx); break;
      // A reading over its base text (furigana).
      case 'ruby': html += `<ruby>${await inline(child(element, 'rubyBase'), ctx)}<rt>${await inline(child(element, 'rt'), ctx)}</rt></ruby>`; break;
      case 'footnoteReference': case 'endnoteReference': {
        // Footnotes 1, 2, 3; endnotes i, ii, iii (as Word numbers them by default).
        const kind = element.localName === 'footnoteReference' ? 'footnote' : 'endnote';
        const count = ctx.referred.filter((one) => one.kind === kind).length + 1;
        const label = kind === 'footnote' ? String(count) : counted(count, 'lowerRoman');
        ctx.referred.push({ kind, id: attr(element, 'id'), label });
        html += `<sup class="ref">${label}</sup>`;
        break;
      }
      default: break;
    }
  }
  return html;
}

/** A picture's crop (`a:srcRect`: shares of each side cut away), or null. */
function cropOf(blip) {
  const rect = child(blip?.parentNode, 'srcRect');
  const side = (local) => Math.min(0.95, Math.max(0, (Number(attr(rect, local)) || 0) / 100000));
  const [l, t, r, b] = ['l', 't', 'r', 'b'].map(side);
  if (!(l || t || r || b) || l + r >= 0.95 || t + b >= 0.95) return null;
  return { l, t, r, b };
}

/** Where a cropped picture's whole image sits in its frame (the frame shows the crop). */
function cropStyle({ l, t, r, b }) {
  const pct = (value) => `${(value * 100).toFixed(3)}%`;
  return `position:absolute;max-width:none;left:${pct(-l / (1 - l - r))};top:${pct(-t / (1 - t - b))};width:${pct(1 / (1 - l - r))};height:${pct(1 / (1 - t - b))}`;
}

/** Whether `node` lies in a text box inside `top` (drawn with the box's text, not again
 * as `top`'s own). */
function inBox(node, top) {
  for (let at = node.parentNode; at && at !== top; at = at.parentNode) if (at.localName === 'txbxContent') return true;
  return false;
}

/** A drawing's pictures (and its text boxes' text); a chart or diagram as a mark. */
async function drawing(node, ctx) {
  let html = '';
  const extent = descendants(node, 'extent')[0];
  const width = extent ? px(attr(extent, 'cx')) : 0;
  const height = extent ? px(attr(extent, 'cy')) : 0;
  const alt = attr(descendants(node, 'docPr')[0], 'descr') ?? '';
  for (const blip of descendants(node, 'blip').filter((found) => !inBox(found, node))) {
    const rel = ctx.rels.get(relAttr(blip, 'embed'));
    const url = rel && !rel.external ? await ctx.pictures(rel.target) : null;
    const crop = cropOf(blip);
    if (!url) html += '<span class="missing">[picture]</span>';
    else if (crop && width > 0 && height > 0) {
      // Cropped: its frame shows only the part the author kept.
      html += `<span class="crop" style="width:${width}px;aspect-ratio:${width}/${height}"><img src="${url}" alt="${esc(alt)}" style="${cropStyle(crop)}"></span>`;
    } else html += `<img src="${url}" alt="${esc(alt)}"${width > 0 ? ` style="width:${width}px"` : ''}>`;
  }
  for (const box of descendants(node, 'txbxContent').filter((found) => !inBox(found, node))) html += `<span class="textbox">${await boxText(box, ctx)}</span>`;
  if (!html && descendants(node, 'chart').length) {
    ctx.notes.add('Charts are not drawn in a preview.');
    html = '<span class="missing">[chart]</span>';
  } else if (!html && descendants(node, 'relIds').length) {
    ctx.notes.add('Diagrams (SmartArt) are not drawn in a preview.');
    html = '<span class="missing">[diagram]</span>';
  }
  return html;
}

/** An older (VML) picture or text box. */
async function vml(node, ctx) {
  let html = '';
  for (const image of descendants(node, 'imagedata').filter((found) => !inBox(found, node))) {
    const rel = ctx.rels.get(relAttr(image, 'id'));
    const url = rel && !rel.external ? await ctx.pictures(rel.target) : null;
    html += url ? `<img src="${url}" alt="">` : '<span class="missing">[picture]</span>';
  }
  for (const box of descendants(node, 'txbxContent').filter((found) => !inBox(found, node))) html += `<span class="textbox">${await boxText(box, ctx)}</span>`;
  return html;
}

/** A text box's paragraphs inline (it sits inside a paragraph), a line each. */
async function boxText(box, ctx) {
  const lines = [];
  for (const p of descendants(box, 'p').filter((found) => !inBox(found, box))) {
    lines.push(await inline(p, { ...ctx, base: ctx.styles.runsOf(val(child(child(p, 'pPr'), 'pStyle'))) }));
  }
  return lines.join('<br>');
}

async function wordTable(tbl, ctx) {
  // Rows and cells, through the wrappers that may hold them.
  const flat = (node, local) => children(node).flatMap((element) => (element.localName === local ? [element]
    : ['sdt', 'customXml'].includes(element.localName) ? flat(element.localName === 'sdt' ? child(element, 'sdtContent') : element, local) : []));
  const rows = flat(tbl, 'tr').map((tr) => {
    let col = Math.max(0, Math.min(1000, Math.trunc(Number(val(child(child(tr, 'trPr'), 'gridBefore')) ?? 0) || 0)));
    return flat(tr, 'tc').map((tc) => {
      const tcPr = child(tc, 'tcPr');
      const span = Math.max(1, Math.min(1000, Math.trunc(Number(val(child(tcPr, 'gridSpan')) ?? 1) || 1)));
      const merge = child(tcPr, 'vMerge');
      const cell = { tc, col, span, rows: 1, merge: merge ? (val(merge) === 'restart' ? 'start' : 'go on') : null, shade: hexColor(attr(child(tcPr, 'shd'), 'fill')) };
      col += span;
      return cell;
    });
  });
  // A cell merged with the one above makes that one taller.
  const above = new Map();
  for (const row of rows) {
    for (const cell of row) {
      if (cell.merge === 'go on' && above.get(cell.col)) {
        above.get(cell.col).rows += 1;
        cell.skip = true;
      } else {
        above.set(cell.col, cell);
      }
    }
  }
  let html = '<table>';
  for (const row of rows) {
    html += '<tr>';
    for (const cell of row) {
      if (cell.skip) continue;
      const span = `${cell.span > 1 ? ` colspan="${cell.span}"` : ''}${cell.rows > 1 ? ` rowspan="${cell.rows}"` : ''}`;
      html += `<td${span}${cell.shade ? ` style="background:${cell.shade}"` : ''}>${(await blocks(cell.tc, ctx)) || '<br>'}</td>`;
    }
    html += '</tr>';
  }
  return `${html}</table>`;
}

// ---- Excel --------------------------------------------------------------------------

async function workbook(zip, main, notes) {
  const book = await xmlPart(zip, main);
  if (!book) throw new Error('its workbook part is missing');
  const rels = await relsOf(zip, main);
  const shared = [];
  for (const si of children((await xmlPart(zip, relation(rels, 'sharedStrings')))?.documentElement, 'si')) shared.push(richText(si));
  const formats = cellFormats(await optional(zip, relation(rels, 'styles')));
  const date1904 = ['1', 'true'].includes(attr(child(book.documentElement, 'workbookPr'), 'date1904') ?? '');
  // Saved asking to be worked out again when opened (as scripts save it): a result its
  // formulas carry may be out of date, or a stand-in (a script saving 0).
  const stale = ['1', 'true'].includes(attr(child(book.documentElement, 'calcPr'), 'fullCalcOnLoad') ?? '');
  const sheets = [];
  for (const sheet of children(child(book.documentElement, 'sheets'), 'sheet')) {
    const state = attr(sheet, 'state');
    if (state === 'veryHidden') continue;
    const rel = rels.get(relAttr(sheet, 'id'));
    if (!rel || rel.external) continue;
    sheets.push({ name: attr(sheet, 'name') ?? `Sheet ${sheets.length + 1}`, hidden: state === 'hidden', rel });
  }
  let html = '';
  for (const [at, sheet] of sheets.entries()) {
    const head = `<h2 id="sheet-${at + 1}">${esc(sheet.name)}${sheet.hidden ? ' <small>(hidden)</small>' : ''}</h2>`;
    if (!/\/worksheet$/.test(sheet.rel.type)) {
      const chart = /\/chartsheet$/.test(sheet.rel.type);
      if (chart) notes.add('Chart sheets are not drawn in a preview.');
      html += `<section class="sheet">${head}<p class="missing">${chart ? 'A chart sheet.' : 'A sheet of another kind (macros or a dialog): not shown.'}</p></section>`;
      continue;
    }
    // A sheet too large to read here says so; the others still show.
    const doc = await xmlPart(zip, sheet.rel.target).catch((error) => error);
    const why = doc instanceof Error ? (doc.kind === 'too_large' ? 'this sheet is too large for a preview; download the file to open it' : doc.message) : null;
    html += `<section class="sheet">${head}${why ? `<p class="missing">Not shown: ${esc(why)}.</p>` : sheetTable(doc, { shared, formats, date1904, stale, notes })}</section>`;
  }
  // More than one sheet: a list of them first (links within the page).
  const index = sheets.length > 1
    ? `<nav class="sheets">${sheets.map((sheet, at) => `<a href="#sheet-${at + 1}">${esc(sheet.name)}</a>`).join('')}</nav>` : '';
  return `${index}${html || '<p class="missing">This workbook has no sheets to show.</p>'}`;
}

/** A shared or inline string: its text, or its runs' (phonetic guides left out), with
 * the characters XML cannot hold that Excel writes as `_xHHHH_` put back. */
function richText(node) {
  const plain = child(node, 't');
  const text = plain ? plain.textContent : children(node, 'r').map((r) => child(r, 't')?.textContent ?? '').join('');
  return text.replace(/_x([0-9A-Fa-f]{4})_/g, (_, hex) => String.fromCharCode(parseInt(hex, 16)));
}

/** Each cell style's number format, as `display` reads it. */
function cellFormats(doc) {
  const builtIn = {
    1: '0', 2: '0.00', 3: '#,##0', 4: '#,##0.00', 9: '0%', 10: '0.00%', 11: '0.00E+00', 14: 'yyyy-mm-dd', 15: 'd-mmm-yy',
    16: 'd-mmm', 17: 'mmm-yy', 18: 'h:mm AM/PM', 19: 'h:mm:ss AM/PM', 20: 'h:mm', 21: 'h:mm:ss', 22: 'yyyy-mm-dd h:mm',
    37: '#,##0', 38: '#,##0', 39: '#,##0.00', 40: '#,##0.00', 45: 'mm:ss', 46: '[h]:mm:ss', 47: 'mm:ss.0', 48: '##0.0E+0', 49: '@',
  };
  const root = doc?.documentElement;
  const custom = new Map(children(child(root, 'numFmts'), 'numFmt').map((format) => [Number(attr(format, 'numFmtId')), attr(format, 'formatCode') ?? '']));
  return children(child(root, 'cellXfs'), 'xf').map((xf) => {
    const id = Number(attr(xf, 'numFmtId') ?? 0);
    return numberFormat(custom.get(id) ?? builtIn[id] ?? (id >= 27 && id <= 36 || id >= 50 && id <= 58 ? 'yyyy-mm-dd' : ''));
  });
}

/** A number format code read into its sections: positive, negative, zero (and text,
 * unused here), or sections chosen by their conditions (`[>=1000]0.0,"K"`). */
function numberFormat(code) {
  if (!code.trim()) return { sections: [{ general: true }] };
  const sections = [];
  let section = '';
  let quoted = false;
  let bracket = false;
  for (let at = 0; at < code.length; at++) {
    const char = code[at];
    if (char === '\\' && !quoted && at + 1 < code.length) {
      section += char + code[++at];
      continue;
    }
    if (char === '"' && !bracket) quoted = !quoted;
    else if (char === '[' && !quoted) bracket = true;
    else if (char === ']' && !quoted) bracket = false;
    if (char === ';' && !quoted && !bracket) {
      sections.push(section);
      section = '';
    } else section += char;
  }
  sections.push(section);
  return { sections: sections.slice(0, 4).map(formatSection) };
}

/** One section of a number format: how it shows a number (`display`), and the condition
 * it is chosen by, if it has one. */
function formatSection(section) {
  const condition = /\[(<=|>=|<>|<|>|=)\s*(-?[\d.]+(?:E[+-]?\d+)?)\]/i.exec(section);
  const plain = section.replace(/"[^"]*"/g, '').replace(/\\./g, '').replace(/[_*]./g, '')
    .replace(/\[[^\]]*\]/g, (found) => (/^\[(h+|m+|s+)\]$/i.test(found) ? found.slice(1, -1) : ''));
  const shape = { condition: condition ? { op: condition[1], value: Number(condition[2]) } : null };
  // What it writes, as written: quotes and escapes read, padding and fill left out, a
  // currency's symbol in `[$€-407]` kept, other brackets (colors, conditions) dropped.
  const shown = section.replace(/"([^"]*)"/g, '$1').replace(/\\(.)/g, '$1').replace(/[_*]./g, '')
    .replace(/\[\$([^-\]]*)(?:-[^\]]*)?\]/g, '$1').replace(/\[[^\]]*\]/g, '');
  // A section of words only (or none: `;;;` hides a number) shows them, not the number.
  if (!plain.trim()) return { ...shape, words: shown };
  if (/^general$/i.test(plain.trim()) || plain.trim() === '@') return { ...shape, general: true };
  if (/[dyhs]|m/i.test(plain) && !/[0#?]/.test(plain.replace(/\.0+/, ''))) {
    return {
      ...shape,
      date: /[dy]/i.test(plain) || (/m/i.test(plain) && !/[hs]/i.test(plain)),
      time: /[hs]/i.test(plain),
      seconds: /s/i.test(plain),
      // Elapsed time ([h]:mm): its hours (minutes, seconds) counted past a day.
      elapsed: /\[h+\]/i.test(section) ? 'hours' : /\[m+\]/i.test(section) ? 'minutes' : /\[s+\]/i.test(section) ? 'seconds' : null,
    };
  }
  // A fraction (`# ?/?`) shows as the number it is, not rounded to a whole one.
  if (plain.includes('/')) return { ...shape, general: true };
  const first = shown.search(/[0#?]/);
  const last = Math.max(shown.lastIndexOf('0'), shown.lastIndexOf('#'), shown.lastIndexOf('?'));
  if (first < 0) return { ...shape, words: shown };
  if (/E[+-]/i.test(plain)) {
    // (At most 20 decimals: what JavaScript formats, and more than Excel shows.)
    return { ...shape, scientific: true, decimals: Math.min(20, /\.([0#?]+)/.exec(plain)?.[1].length ?? 0) };
  }
  const number = shown.slice(first, last + 1);
  let suffix = shown.slice(last + 1);
  // Commas right after the last digit scale by a thousand each (`#,##0,` shows
  // thousands, `0.0,,"M"` millions); at most six.
  const scaling = /^,+/.exec(suffix)?.[0].length ?? 0;
  suffix = suffix.slice(scaling);
  const point = number.indexOf('.');
  const whole = point < 0 ? number : number.slice(0, point);
  const fraction = point < 0 ? null : number.slice(point + 1).replace(/[^0#?]/g, '');
  return {
    ...shape,
    percent: shown.includes('%'),
    scale: 1000 ** Math.min(6, scaling),
    grouped: /[0#?],+[0#?]/.test(whole),
    // The whole part's pattern (its digits and the literals among them, `000-00-0000`),
    // grouping commas aside.
    whole: whole.replace(/,/g, ''),
    decimals: fraction == null ? 0 : Math.min(20, fraction.length),
    required: fraction == null ? 0 : Math.min(20, (fraction.match(/0/g) ?? []).length),
    point: point >= 0,
    prefix: shown.slice(0, first),
    suffix,
  };
}

/** A number as its format shows it, roughly (dates as ISO dates): the section its
 * conditions, or its sign, choose. */
function display(n, format, date1904) {
  if (!format) return trimmed(n);
  const { sections } = format;
  let section;
  let sign = n < 0;
  if (sections.some((one) => one.condition)) {
    const holds = ({ op, value }) => own({ '<': n < value, '>': n > value, '=': n === value, '<=': n <= value, '>=': n >= value, '<>': n !== value }, op);
    section = sections.slice(0, 2).find((one) => one.condition && holds(one.condition)) ?? sections.find((one, at) => at >= 2 || !one.condition) ?? null;
    if (!section) return trimmed(n);
  } else if (n < 0 && sections.length > 1) {
    // A section for negative numbers writes them its own way (a minus, brackets, red).
    section = sections[1];
    sign = false;
  } else if (n === 0 && sections.length > 2) section = sections[2];
  else section = sections[0];
  if (section.words != null) return section.words;
  return shownBy(section, sign ? -Math.abs(n) : Math.abs(n), date1904);
}

/** `n` as one section of a format shows it. */
function shownBy(format, n, date1904) {
  if (format.general) return trimmed(n);
  if (format.date || format.time) {
    if (format.elapsed) {
      // Hours (minutes, seconds) as many as there are, then the rest of the clock.
      const total = Math.round(Math.abs(n) * 86400);
      const head = format.elapsed === 'hours' ? Math.floor(total / 3600) : format.elapsed === 'minutes' ? Math.floor(total / 60) : total;
      const minutes = format.elapsed === 'hours' ? `:${String(Math.floor(total / 60) % 60).padStart(2, '0')}` : '';
      const seconds = format.seconds && format.elapsed !== 'seconds' ? `:${String(total % 60).padStart(2, '0')}` : '';
      return `${n < 0 ? '-' : ''}${head}${minutes}${seconds}`;
    }
    // Before Excel's first day or past its last (9999-12-31), it shows hashes.
    if (n < 0 || n >= 2958466) return '#####';
    // Excel counts a 29 February 1900 that never was (its day 60), and so is one day
    // ahead before it.
    if (!date1904 && Math.floor(n) === 60) return format.time && format.date ? `1900-02-29 ${clock(n, format)}` : format.date ? '1900-02-29' : clock(n, format);
    const days = date1904 ? n + 1462 : n < 60 ? n + 1 : n;
    const iso = new Date(Math.round((days - 25569) * 86400000)).toISOString();
    return format.date && format.time ? `${iso.slice(0, 10)} ${clock(n, format)}` : format.date ? iso.slice(0, 10) : clock(n, format);
  }
  // As Excel writes it: two digits of exponent at least (1.23E+03).
  if (format.scientific) return n.toExponential(format.decimals).toUpperCase().replace(/E([+-])(\d)$/, 'E$10$2');
  const value = Math.abs((format.percent ? n * 100 : n) / format.scale);
  const fixed = value.toFixed(format.decimals);
  // Past what fixed notation writes (1e21 and up): as the number it is.
  if (!/^\d+(\.\d+)?$/.test(fixed)) return trimmed(n);
  const [digits, fraction = ''] = fixed.split('.');
  // Its decimals, the optional ones (`#`) without their trailing zeros.
  let decimals = fraction;
  while (decimals.length > format.required && decimals.endsWith('0')) decimals = decimals.slice(0, -1);
  // The whole part into its pattern, digit by digit from the right (`000-00-0000`); a
  // zero where `0` asks for one, nothing where `#` does; digits left over go first.
  const pattern = format.whole;
  let left = digits === '0' && !pattern.includes('0') ? '' : digits;
  let whole = '';
  for (let at = pattern.length - 1; at >= 0; at--) {
    const char = pattern[at];
    if (char === '0' || char === '#' || char === '?') {
      whole = (left ? left.slice(-1) : char === '0' ? '0' : char === '?' ? ' ' : '') + whole;
      left = left.slice(0, -1);
    } else whole = char + whole;
  }
  whole = left + whole;
  if (format.grouped) whole = whole.replace(/\d+/, (run) => run.replace(/\B(?=(\d{3})+(?!\d))/g, ','));
  const negative = n < 0 && Number(fixed) !== 0;
  return `${negative ? '-' : ''}${format.prefix}${whole}${format.point ? `.${decimals}` : ''}${format.suffix}`;
}

/** A date a cell holds as ISO text (`t="d"`), as its format shows it: its day, its time,
 * or both (as written, where the format is no date's). */
function isoShown(value, format) {
  const section = format?.sections?.[0];
  if (!/^\d{4}-\d\d-\d\d/.test(value) || !(section?.date || section?.time)) return value;
  const [day, time = '00:00:00'] = value.split('T');
  const clockShown = time.slice(0, section.seconds ? 8 : 5);
  return section.date && section.time ? `${day} ${clockShown}` : section.date ? day : clockShown;
}

/** A time of day as its format shows it (seconds if it shows them). */
function clock(n, format) {
  const seconds = Math.round((n - Math.floor(n)) * 86400) % 86400;
  const parts = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60].map((part) => String(part).padStart(2, '0'));
  return format.seconds ? parts.join(':') : parts.slice(0, 2).join(':');
}

const trimmed = (n) => (Number.isInteger(n) ? String(n) : String(Number(n.toPrecision(15))));

/** Column number of a cell reference (`C7` -> 3). */
function columnOf(ref) {
  let n = 0;
  for (const char of /^[A-Z]+/i.exec(ref)?.[0].toUpperCase() ?? '') n = n * 26 + char.charCodeAt(0) - 64;
  return n;
}
function columnName(n) {
  let name = '';
  for (; n > 0; n = Math.floor((n - 1) / 26)) name = String.fromCharCode(65 + ((n - 1) % 26)) + name;
  return name;
}

function sheetTable(doc, { shared, formats, date1904, stale, notes }) {
  const root = doc?.documentElement;
  const cells = new Map();
  let formulas = false;
  let rows = 0;
  let columns = 0;
  let cut = false;
  let r = 0;
  for (const row of children(child(root, 'sheetData'), 'row')) {
    r = Number(attr(row, 'r')) || r + 1;
    if (r > MAX_ROWS) {
      cut = true;
      break;
    }
    let col = 0;
    for (const c of children(row, 'c')) {
      const ref = attr(c, 'r');
      col = ref ? columnOf(ref) : col + 1;
      if (col > MAX_COLUMNS) {
        cut = true;
        continue;
      }
      const type = attr(c, 't') ?? 'n';
      const value = child(c, 'v')?.textContent ?? '';
      const formula = child(c, 'f');
      let text = value;
      let number = false;
      // (A formula that gave an empty text is `t="str"` with an empty value: empty.)
      if (value === '' && formula && type === 'n') {
        // Saved without its result (as scripts often save one): the formula itself, or
        // a mark where it shares another cell's.
        text = formula.textContent ? `=${formula.textContent}` : '=…';
        formulas = true;
      } else {
        if (formula && stale) notes.add("This workbook asks to be recalculated when opened: its formulas' results here may be out of date.");
        if (type === 's') text = shared[Number(value)] ?? '';
        else if (type === 'inlineStr') text = richText(child(c, 'is'));
        else if (type === 'b') text = value === '1' ? 'TRUE' : value === '0' ? 'FALSE' : value;
        else if (type === 'd') text = isoShown(value, formats[Number(attr(c, 's') ?? 0)]);
        else if (type === 'n' && value !== '') {
          const n = Number(value);
          if (Number.isFinite(n)) {
            text = display(n, formats[Number(attr(c, 's') ?? 0)], date1904);
            number = true;
          }
        }
      }
      if (text === '') continue;
      cells.set(`${r}:${col}`, { text, number });
      rows = Math.max(rows, r);
      columns = Math.max(columns, col);
    }
  }
  if (formulas) notes.add('Some formulas were saved without their results: they show as formulas.');
  if (!rows) return '<p class="missing">Empty.</p>';
  if (rows * columns > MAX_CELLS) {
    rows = Math.max(1, Math.floor(MAX_CELLS / columns));
    cut = true;
  }
  if (cut) notes.add(`A sheet shows its first ${MAX_ROWS.toLocaleString('en-US')} rows and ${MAX_COLUMNS} columns at most (${MAX_CELLS.toLocaleString('en-US')} cells).`);
  // Merged cells: the first holds the rest.
  const spans = new Map();
  const covered = new Set();
  for (const merge of children(child(root, 'mergeCells'), 'mergeCell')) {
    const [from, to = from] = (attr(merge, 'ref') ?? '').split(':');
    const [r1, c1, r2, c2] = [Number(/\d+$/.exec(from)?.[0]), columnOf(from), Number(/\d+$/.exec(to)?.[0]), columnOf(to)];
    if (!(r1 && c1 && r2 >= r1 && c2 >= c1)) continue;
    spans.set(`${r1}:${c1}`, { rows: Math.min(r2, rows) - r1 + 1, columns: Math.min(c2, columns) - c1 + 1 });
    for (let y = r1; y <= r2 && y <= rows; y++) for (let x = c1; x <= c2 && x <= columns; x++) if (y !== r1 || x !== c1) covered.add(`${y}:${x}`);
  }
  let html = '<div class="scroll"><table class="grid"><thead><tr><th></th>';
  for (let x = 1; x <= columns; x++) html += `<th>${columnName(x)}</th>`;
  html += '</tr></thead><tbody>';
  for (let y = 1; y <= rows; y++) {
    html += `<tr><th>${y}</th>`;
    for (let x = 1; x <= columns; x++) {
      const key = `${y}:${x}`;
      if (covered.has(key)) continue;
      const cell = cells.get(key);
      const span = spans.get(key);
      const attrs = `${span?.columns > 1 ? ` colspan="${span.columns}"` : ''}${span?.rows > 1 ? ` rowspan="${span.rows}"` : ''}${cell?.number ? ' class="num"' : ''}`;
      html += `<td${attrs}>${cell ? esc(cell.text) : ''}</td>`;
    }
    html += '</tr>';
  }
  return `${html}</tbody></table></div>`;
}

// ---- PowerPoint ---------------------------------------------------------------------

async function deck(zip, main, pictures, notes) {
  const pres = await xmlPart(zip, main);
  if (!pres) throw new Error('its presentation part is missing');
  const rels = await relsOf(zip, main);
  const size = child(pres.documentElement, 'sldSz');
  const W = Number(attr(size, 'cx')) || 12192000;
  const H = Number(attr(size, 'cy')) || 6858000;
  const layouts = new Map();
  const masters = new Map();
  const masterOf = async (name) => {
    if (!masters.has(name)) {
      const xml = await optional(zip, name);
      const masterRels = await relsOf(zip, name).catch(() => new Map());
      const theme = await optional(zip, relation(masterRels, 'theme'));
      const scheme = new Map();
      for (const color of children(descendants(theme?.documentElement, 'clrScheme')[0])) {
        const value = child(color, 'srgbClr') ? val(child(color, 'srgbClr')) : attr(child(color, 'sysClr'), 'lastClr');
        if (hexColor(value)) scheme.set(color.localName, value.toLowerCase());
      }
      const map = new Map();
      for (const attribute of child(xml?.documentElement, 'clrMap')?.attributes ?? []) map.set(attribute.localName, attribute.value);
      masters.set(name, { xml, rels: masterRels, colors: { scheme, map } });
    }
    return masters.get(name);
  };
  const layoutOf = async (name) => {
    if (!layouts.has(name)) {
      const xml = await optional(zip, name);
      const layoutRels = await relsOf(zip, name).catch(() => new Map());
      const masterName = relation(layoutRels, 'slideMaster');
      layouts.set(name, { xml, rels: layoutRels, master: masterName ? await masterOf(masterName) : null });
    }
    return layouts.get(name);
  };
  let html = '';
  let n = 0;
  for (const id of children(child(pres.documentElement, 'sldIdLst'), 'sldId')) {
    if (n >= MAX_SLIDES) {
      notes.add(`A preview shows the first ${MAX_SLIDES} slides.`);
      break;
    }
    const rel = rels.get(relAttr(id, 'id'));
    if (!rel || rel.external) continue;
    n += 1;
    const slide = await xmlPart(zip, rel.target);
    const slideRels = await relsOf(zip, rel.target);
    const layoutName = relation(slideRels, 'slideLayout');
    const layout = layoutName ? await layoutOf(layoutName) : null;
    const master = layout?.master ?? null;
    // A slide (or its layout) may map the theme's colors its own way (a dark layout).
    const overrideOf = (xml) => child(child(xml?.documentElement, 'clrMapOvr'), 'overrideClrMapping');
    const override = overrideOf(slide) ?? overrideOf(layout?.xml);
    const colors = master?.colors ?? { scheme: new Map(), map: new Map() };
    const ctx = {
      W, H, pictures, notes, rels: slideRels,
      colors: override ? { scheme: colors.scheme, map: new Map([...override.attributes].map((one) => [one.localName, one.value])) } : colors,
      places: places(layout?.xml, master?.xml),
      styles: textStyles(master?.xml),
    };
    ctx.color = (node) => colorOf(node, ctx.colors);
    const background = backgroundOf(slide, ctx) ?? backgroundOf(layout?.xml, ctx) ?? backgroundOf(master?.xml, ctx) ?? '#ffffff';
    const tree = child(child(slide?.documentElement, 'cSld'), 'spTree');
    const shapes = await shapeTree(tree, ctx, { sx: 1, sy: 1, dx: 0, dy: 0 });
    const hidden = attr(slide?.documentElement, 'show') === '0';
    html += `<figure class="slide-box"><figcaption>Slide ${n}${hidden ? ' (hidden)' : ''}</figcaption>`
      + `<div class="slide" style="aspect-ratio:${W}/${H};background:${background}">${shapes}</div></figure>`;
  }
  return html || '<p class="missing">This presentation has no slides.</p>';
}

/** What a slide's placeholder takes from its layout's (by index, else type) and its
 * master's (by type): where it sits when the slide does not say, its text's list styles
 * (sizes, colors; the layout's first) and where its text sits. */
function places(layout, master) {
  const family = (type) => own({ ctrTitle: 'title', subTitle: 'body', obj: 'body' }, type) ?? type;
  const table = (xml) => {
    const byIdx = new Map();
    const byType = new Map();
    for (const sp of descendants(xml?.documentElement, 'sp')) {
      const ph = descendants(child(sp, 'nvSpPr'), 'ph')[0];
      if (!ph) continue;
      const body = child(sp, 'txBody');
      const entry = { box: xfrmOf(child(child(sp, 'spPr'), 'xfrm')), list: child(body, 'lstStyle'), anchor: attr(child(body, 'bodyPr'), 'anchor') };
      const type = attr(ph, 'type') ?? 'body';
      if (attr(ph, 'idx') != null && !byIdx.has(attr(ph, 'idx'))) byIdx.set(attr(ph, 'idx'), entry);
      if (!byType.has(type)) byType.set(type, entry);
      if (!byType.has(family(type))) byType.set(family(type), entry);
    }
    return { byIdx, byType };
  };
  const [fromLayout, fromMaster] = [table(layout), table(master)];
  return (ph) => {
    const type = attr(ph, 'type') ?? 'body';
    const idx = attr(ph, 'idx');
    const mine = (idx != null ? fromLayout.byIdx.get(idx) : null) ?? fromLayout.byType.get(type) ?? fromLayout.byType.get(family(type)) ?? null;
    const theirs = fromMaster.byType.get(type) ?? fromMaster.byType.get(family(type)) ?? null;
    return {
      box: mine?.box ?? theirs?.box ?? null,
      lists: [mine?.list, theirs?.list].filter(Boolean),
      anchor: mine?.anchor ?? theirs?.anchor ?? null,
    };
  };
}

/** A master's text styles: each level's size (points) and color, for titles, bodies and
 * other text. */
function textStyles(master) {
  const styles = child(master?.documentElement, 'txStyles');
  const read = (local) => {
    const levels = [];
    const node = child(styles, local);
    for (let level = 1; level <= 9; level++) {
      const pPr = child(node, `lvl${level}pPr`);
      const defRPr = child(pPr, 'defRPr');
      levels.push({ pPr, size: Number(attr(defRPr, 'sz')) / 100 || null, fill: child(defRPr, 'solidFill') });
    }
    return levels;
  };
  return { title: read('titleStyle'), body: read('bodyStyle'), other: read('otherStyle') };
}

function xfrmOf(xfrm) {
  const off = child(xfrm, 'off');
  const ext = child(xfrm, 'ext');
  if (!off || !ext) return null;
  return {
    x: Number(attr(off, 'x')) || 0, y: Number(attr(off, 'y')) || 0,
    cx: Number(attr(ext, 'cx')) || 0, cy: Number(attr(ext, 'cy')) || 0,
    rot: (Number(attr(xfrm, 'rot')) || 0) / 60000,
    chOff: child(xfrm, 'chOff'), chExt: child(xfrm, 'chExt'),
  };
}

/** A color node's parent (`solidFill`, a style ref) as CSS: RGB, a theme color (through
 * the master's map), a system color's last value, a named one; lightened or darkened as
 * it says. */
function colorOf(parent, colors) {
  const node = children(parent).find((element) => /^(srgbClr|schemeClr|sysClr|prstClr|scrgbClr)$/.test(element.localName));
  if (!node) return null;
  let hex = null;
  if (node.localName === 'srgbClr') hex = val(node);
  else if (node.localName === 'sysClr') hex = attr(node, 'lastClr');
  else if (node.localName === 'schemeClr') {
    const name = val(node);
    hex = colors.scheme.get(colors.map.get(name) ?? own({ bg1: 'lt1', tx1: 'dk1', bg2: 'lt2', tx2: 'dk2' }, name) ?? name) ?? null;
  } else if (node.localName === 'prstClr') {
    return /^[a-z]+$/i.test(val(node) ?? '') ? val(node).toLowerCase() : null;
  }
  if (!hexColor(hex)) return null;
  // A modifier as a share (100000 is all), or `otherwise` where the color has none.
  const part = (local, otherwise) => {
    const raw = val(child(node, local));
    const value = raw == null ? NaN : Number(raw);
    return Number.isFinite(value) ? value / 100000 : otherwise;
  };
  const mod = part('lumMod', 1);
  const off = part('lumOff', 0);
  let color = mod === 1 && off === 0 ? `#${hex.toLowerCase()}` : lightness(hex, mod, off);
  // A tint toward white, a shade toward black (each the share of the color kept).
  const tint = part('tint', null);
  const shade = part('shade', null);
  if (tint != null || shade != null) {
    const mixed = [1, 3, 5].map((at) => {
      let channel = parseInt(color.slice(at, at + 2), 16);
      if (shade != null) channel *= Math.min(1, Math.max(0, shade));
      if (tint != null) channel = channel * Math.min(1, Math.max(0, tint)) + 255 * (1 - Math.min(1, Math.max(0, tint)));
      return Math.round(channel).toString(16).padStart(2, '0');
    });
    color = `#${mixed.join('')}`;
  }
  // See-through: as see-through (none at all: no color).
  const alpha = part('alpha', 1);
  if (alpha <= 0) return null;
  if (alpha >= 1) return color;
  const [r, g, b] = [1, 3, 5].map((at) => parseInt(color.slice(at, at + 2), 16));
  return `rgba(${r},${g},${b},${alpha.toFixed(3)})`;
}

/** `hex` with its lightness times `mod` plus `off` (as DrawingML's lumMod and lumOff). */
function lightness(hex, mod, off) {
  const [r, g, b] = [0, 2, 4].map((at) => parseInt(hex.slice(at, at + 2), 16) / 255);
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  let l = (max + min) / 2;
  const d = max - min;
  const s = d === 0 ? 0 : d / (1 - Math.abs(2 * l - 1));
  let h = 0;
  if (d) h = max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  // Red's sector wraps: a hue just below red is near 6, not below 0.
  if (h < 0) h += 6;
  l = Math.min(1, Math.max(0, l * mod + off));
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const x = c * (1 - Math.abs((h % 2 + 2) % 2 - 1));
  const m = l - c / 2;
  const [r1, g1, b1] = h < 1 && h >= 0 ? [c, x, 0] : h < 2 ? [x, c, 0] : h < 3 ? [0, c, x] : h < 4 ? [0, x, c] : h < 5 ? [x, 0, c] : [c, 0, x];
  return `#${[r1, g1, b1].map((v) => Math.round((v + m) * 255).toString(16).padStart(2, '0')).join('')}`;
}

/** A slide's (layout's, master's) background color, if it has one. */
function backgroundOf(xml, ctx) {
  const bg = child(child(xml?.documentElement, 'cSld'), 'bg');
  if (!bg) return null;
  const props = child(bg, 'bgPr');
  const gradient = descendants(props, 'gs')[0];
  return ctx.color(child(props, 'solidFill')) ?? (gradient ? ctx.color(gradient) : null) ?? ctx.color(child(bg, 'bgRef'));
}

/** A shape tree (a slide's, or a group's) as positioned HTML; `t` maps its coordinates
 * to the slide's. */
async function shapeTree(tree, ctx, t) {
  let html = '';
  for (const element of children(tree)) {
    const hidden = attr(descendants(element, 'cNvPr')[0], 'hidden');
    if (hidden === '1' || hidden === 'true') continue;
    switch (element.localName) {
      case 'sp': html += shape(element, ctx, t); break;
      case 'pic': html += await slidePicture(element, ctx, t); break;
      case 'graphicFrame': html += graphicFrame(element, ctx, t); break;
      case 'grpSp': {
        const box = xfrmOf(child(child(element, 'grpSpPr'), 'xfrm'));
        if (!box) {
          html += await shapeTree(element, ctx, t);
          break;
        }
        const chOff = { x: Number(attr(box.chOff, 'x')) || 0, y: Number(attr(box.chOff, 'y')) || 0 };
        const chExt = { cx: Number(attr(box.chExt, 'cx')) || box.cx || 1, cy: Number(attr(box.chExt, 'cy')) || box.cy || 1 };
        const kx = box.cx / chExt.cx;
        const ky = box.cy / chExt.cy;
        html += await shapeTree(element, ctx, {
          sx: t.sx * kx, sy: t.sy * ky,
          dx: t.sx * (box.x - chOff.x * kx) + t.dx, dy: t.sy * (box.y - chOff.y * ky) + t.dy,
        });
        break;
      }
      case 'AlternateContent': html += await shapeTree(choice(element), ctx, t); break;
      default: break;
    }
  }
  return html;
}

/** Where a box goes on the slide, as CSS (percentages of the slide). */
function placed(box, ctx, t) {
  const pct = (value, of) => `${((value / of) * 100).toFixed(3)}%`;
  return `left:${pct(box.x * t.sx + t.dx, ctx.W)};top:${pct(box.y * t.sy + t.dy, ctx.H)};width:${pct(box.cx * t.sx, ctx.W)};height:${pct(box.cy * t.sy, ctx.H)}`
    + (box.rot ? `;transform:rotate(${box.rot.toFixed(2)}deg)` : '');
}

/** A size in points as container units (the slide is 100cqw wide). */
const sized = (points, ctx) => `${((points / (ctx.W / 12700)) * 100).toFixed(3)}cqw`;

function shape(sp, ctx, t) {
  const spPr = child(sp, 'spPr');
  const ph = descendants(child(sp, 'nvSpPr'), 'ph')[0] ?? null;
  const inherited = ph ? ctx.places(ph) : { box: null, lists: [], anchor: null };
  const box = xfrmOf(child(spPr, 'xfrm')) ?? inherited.box;
  const role = ph ? attr(ph, 'type') ?? 'body' : null;
  const text = textBody(child(sp, 'txBody'), ctx, role, inherited.lists);
  const fill = ctx.color(child(spPr, 'solidFill'));
  if (!text && !fill) return '';
  const where = box ? placed(box, ctx, t) : 'left:5%;top:5%;width:90%';
  const anchor = attr(child(child(sp, 'txBody'), 'bodyPr'), 'anchor') ?? inherited.anchor ?? (role === 'title' || role === 'ctrTitle' ? 'ctr' : 't');
  const justify = own({ ctr: 'center', b: 'flex-end' }, anchor) ?? 'flex-start';
  return `<div class="shape" style="${where};justify-content:${justify}${fill ? `;background:${fill}` : ''}">${text}</div>`;
}

/** Placeholders whose text is a slide's small print (date, footer, number). */
const SMALL = new Set(['dt', 'ftr', 'sldNum', 'hdr']);

/** A text body's paragraphs: sizes and colors as the run says, else the body's list
 * style, its placeholder's on the layout and master (`lists`), the master's text styles;
 * bullets in body placeholders unless turned off, else as written. */
function textBody(body, ctx, role, lists = []) {
  if (!body) return '';
  const scale = Number(attr(child(child(body, 'bodyPr'), 'normAutofit'), 'fontScale') ?? 100000) / 100000 || 1;
  const kind = role === 'title' || role === 'ctrTitle' ? 'title' : SMALL.has(role) || !role ? 'other' : 'body';
  const styles = [child(body, 'lstStyle'), ...lists];
  const counters = [];
  let html = '';
  for (const p of children(body, 'p')) {
    const pPr = child(p, 'pPr');
    const level = Math.max(0, Math.min(8, Math.trunc(Number(attr(pPr, 'lvl')) || 0)));
    const levels = [pPr, ...styles.map((list) => child(list, `lvl${level + 1}pPr`))].filter(Boolean);
    const defaults = levels.map((props) => child(props, 'defRPr')).filter(Boolean);
    const style = ctx.styles[kind][level];
    const listed = defaults.map((defRPr) => Number(attr(defRPr, 'sz')) / 100).find((size) => size > 0);
    const base = listed || (SMALL.has(role) ? 12 : style?.size) || (kind === 'title' ? 40 : kind === 'body' ? Math.max(14, 24 - 2 * level) : 18);
    const color = defaults.map((defRPr) => ctx.color(child(defRPr, 'solidFill'))).find(Boolean) ?? ctx.color(style?.fill) ?? null;
    let runs = '';
    let first = null;
    for (const element of children(p)) {
      if (element.localName === 'br') {
        runs += '<br>';
        continue;
      }
      if (element.localName !== 'r' && element.localName !== 'fld') continue;
      const rPr = child(element, 'rPr');
      const size = Number(attr(rPr, 'sz')) / 100 || base;
      // The paragraph takes its first run's size; a run of another size says its own.
      first ??= size;
      const runColor = ctx.color(child(rPr, 'solidFill'));
      const css = [
        size !== first && `font-size:${sized(size * scale, ctx)}`,
        runColor && `color:${runColor}`,
        ['1', 'true'].includes(attr(rPr, 'b') ?? '') && 'font-weight:bold',
        ['1', 'true'].includes(attr(rPr, 'i') ?? '') && 'font-style:italic',
        (attr(rPr, 'u') ?? 'none') !== 'none' && 'text-decoration:underline',
      ].filter(Boolean).join(';');
      const text = esc(child(element, 't')?.textContent ?? '');
      runs += css ? `<span style="${css}">${text}</span>` : text;
    }
    // A bullet: as the paragraph says, else its placeholder's list styles, else the
    // master's (a body placeholder's text is bulleted unless one turns it off).
    let mark = '';
    const told = [...levels, style?.pPr].find((props) => props && ['buNone', 'buChar', 'buAutoNum'].some((local) => child(props, local)));
    const auto = child(told, 'buAutoNum');
    if (runs && !child(told, 'buNone') && (told || (kind === 'body' && role !== 'subTitle'))) {
      if (child(told, 'buChar')) mark = bullet(attr(child(told, 'buChar'), 'char'));
      else if (auto) {
        counters[level] = Math.min(MAX_START, (counters[level] ?? Math.min(MAX_START, Math.max(1, Number(attr(auto, 'startAt')) || 1)) - 1) + 1);
        counters.length = level + 1;
        const type = attr(auto, 'type') ?? 'arabicPeriod';
        const number = counted(counters[level], /alphaLc/.test(type) ? 'lowerLetter' : /alphaUc/.test(type) ? 'upperLetter' : /romanLc/.test(type) ? 'lowerRoman' : /romanUc/.test(type) ? 'upperRoman' : 'decimal');
        mark = /ParenBoth/.test(type) ? `(${number})` : /ParenR/.test(type) ? `${number})` : `${number}.`;
      } else mark = '•';
    }
    // Its alignment as its bullet: the paragraph's, else its list styles', else the master's.
    const align = own({ ctr: 'center', r: 'right', just: 'justify', dist: 'justify' }, [...levels, style?.pPr].map((props) => attr(props, 'algn')).find(Boolean)) ?? null;
    const css = [
      `font-size:${sized((first ?? base) * scale, ctx)}`,
      color && `color:${color}`,
      align && `text-align:${align}`,
      level && `margin-left:${(level * 4).toFixed(1)}%`,
    ].filter(Boolean).join(';');
    html += `<p style="${css}">${mark ? `<span class="mark">${esc(mark)}</span>` : ''}${runs || '&nbsp;'}</p>`;
  }
  return html.replace(/(?:<p style="[^"]*">&nbsp;<\/p>)+$/, '');
}

async function slidePicture(pic, ctx, t) {
  const blip = descendants(child(pic, 'blipFill'), 'blip')[0];
  const rel = ctx.rels.get(relAttr(blip, 'embed'));
  const ph = descendants(child(pic, 'nvPicPr'), 'ph')[0];
  const box = xfrmOf(child(child(pic, 'spPr'), 'xfrm')) ?? (ph ? ctx.places(ph).box : null);
  const url = rel && !rel.external ? await ctx.pictures(rel.target) : null;
  if (!box) return '';
  const alt = attr(descendants(pic, 'cNvPr')[0], 'descr') ?? '';
  if (!url) return `<div class="shape missing" style="${placed(box, ctx, t)}">[picture]</div>`;
  const crop = cropOf(blip);
  // Cropped: its frame shows only the part the author kept.
  if (crop) return `<div class="shape crop" style="${placed(box, ctx, t)}"><img src="${url}" alt="${esc(alt)}" style="${cropStyle(crop)}"></div>`;
  return `<img class="shape picture" style="${placed(box, ctx, t)}" src="${url}" alt="${esc(alt)}">`;
}

/** A table on a slide; a chart or diagram as a mark. */
function graphicFrame(frame, ctx, t) {
  const box = xfrmOf(child(frame, 'xfrm'));
  if (!box) return '';
  const table = descendants(frame, 'tbl')[0];
  if (!table) {
    const what = descendants(frame, 'chart').length ? 'chart' : descendants(frame, 'relIds').length ? 'diagram' : null;
    if (!what) return '';
    ctx.notes.add(what === 'chart' ? 'Charts are not drawn in a preview.' : 'Diagrams (SmartArt) are not drawn in a preview.');
    return `<div class="shape missing" style="${placed(box, ctx, t)};justify-content:center">[${what}]</div>`;
  }
  let html = '<table>';
  for (const tr of children(table, 'tr')) {
    html += '<tr>';
    for (const tc of children(tr, 'tc')) {
      if (['1', 'true'].includes(attr(tc, 'hMerge') ?? '') || ['1', 'true'].includes(attr(tc, 'vMerge') ?? '')) continue;
      const across = Math.min(1000, Math.trunc(Number(attr(tc, 'gridSpan')) || 1));
      const down = Math.min(1000, Math.trunc(Number(attr(tc, 'rowSpan')) || 1));
      const span = `${across > 1 ? ` colspan="${across}"` : ''}${down > 1 ? ` rowspan="${down}"` : ''}`;
      const fill = ctx.color(child(child(tc, 'tcPr'), 'solidFill'));
      html += `<td${span}${fill ? ` style="background:${fill}"` : ''}>${textBody(child(tc, 'txBody'), ctx, null)}</td>`;
    }
    html += '</tr>';
  }
  return `<div class="shape" style="${placed(box, ctx, t)}">${html}</table></div>`;
}

// ---- Looks --------------------------------------------------------------------------

const FONT = "Calibri, Carlito, Aptos, 'Segoe UI', Arial, sans-serif";
const BASE = `:root{color-scheme:light}body{margin:0;background:#e8e8e8;color:#1b1b1b;font:15px/1.5 ${FONT}}
.missing{color:#777;font-style:italic}img{max-width:100%;height:auto}`;
const CSS = {
  doc: `${BASE}
.page{background:#fff;max-width:816px;margin:16px auto;padding:56px 64px;box-sizing:border-box;box-shadow:0 1px 4px rgba(0,0,0,.2);overflow-wrap:anywhere}
@media (max-width:640px){.page{margin:0;padding:16px 18px;box-shadow:none}}
p{margin:0 0 .55em}h1,h2,h3,h4,h5,h6{margin:.9em 0 .4em;line-height:1.25;color:#1f3864}h1{font-size:1.75em}h2{font-size:1.4em}h3{font-size:1.2em}h4,h5,h6{font-size:1.05em}
h1.title{font-size:2.3em;font-weight:500;color:#1b1b1b}.subtitle{color:#5a5a5a;font-size:1.15em}
.li{margin-top:0;margin-bottom:.2em}.mark{display:inline-block;min-width:1.6em;margin-left:-1.6em;padding-right:.35em;box-sizing:border-box}
.tab{display:inline-block;width:2em}.link{color:#1a56b0;text-decoration:underline}.math{font-family:'Cambria Math',serif}
.page{white-space:pre-wrap}.crop{display:inline-block;position:relative;overflow:hidden;max-width:100%;vertical-align:bottom}
.textbox{display:inline-block;border:1px solid #ccc;padding:4px 8px;margin:4px 0}
table{border-collapse:collapse;margin:.6em 0;max-width:100%}td{border:1px solid #bfbfbf;padding:4px 7px;vertical-align:top}td p{margin:0}
.ref{font-size:.7em}.notes{border-top:1px solid #ccc;margin-top:2em;padding-top:.6em;font-size:.88em}.notes .note{display:flex;gap:.4em}.notes .note p{margin:0}`,
  xls: `${BASE}
body{padding:12px;font-size:13px}h2{font-size:15px;margin:18px 0 6px}h2 small{color:#777;font-weight:normal}
.sheets{display:flex;flex-wrap:wrap;gap:6px}.sheets a{background:#fff;border:1px solid #c8c8c8;border-radius:4px;padding:2px 8px;color:#1b1b1b;text-decoration:none}
.scroll{overflow:auto;max-width:100%;max-height:85vh;background:#fff;border:1px solid #c8c8c8}
.grid{border-collapse:collapse;font-variant-numeric:tabular-nums}.grid td,.grid th{border:1px solid #dadada;padding:2px 6px;white-space:pre-wrap;max-width:32em;vertical-align:top}
.grid th{background:#f1f1f1;color:#555;font-weight:normal}.grid thead th{position:sticky;top:0;z-index:1}.grid tbody th{position:sticky;left:0;text-align:right}.grid td.num{text-align:right}`,
  ppt: `${BASE}
body{padding:12px}.slide-box{max-width:960px;margin:0 auto 18px}.slide-box figcaption{font-size:12px;color:#555;margin:0 0 4px}
.slide{position:relative;width:100%;container-type:inline-size;overflow:hidden;box-shadow:0 1px 4px rgba(0,0,0,.25);font-family:${FONT}}
.shape{position:absolute;display:flex;flex-direction:column;box-sizing:border-box;padding:.4cqw .75cqw;overflow:visible}
.shape p{margin:0;line-height:1.15;overflow-wrap:anywhere;white-space:pre-wrap}.shape .mark{display:inline-block;min-width:1.2em}
.shape.crop{padding:0;overflow:hidden}
.picture{padding:0;object-fit:contain}.shape table{border-collapse:collapse;width:100%;font-size:1.5cqw}.shape td{border:1px solid #bbb;padding:.3cqw .5cqw;vertical-align:top}
.shape.missing{border:1px dashed #aaa;align-items:center}`,
};
