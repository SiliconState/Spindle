// Markdown drawn into the page: text.js's blocks (store/ui/text.js, the native UI's
// parser) as elements, as the native UI draws them, without its workspace links. The
// panel's messages and file cards and a file's tab share it.

const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};

function inlineNodes(parent, runs) {
  for (const run of runs) {
    if (run.type === 'text') parent.append(document.createTextNode(run.text));
    else if (run.type === 'code') parent.append(el('code', '', run.text));
    else if (run.type === 'link') {
      const a = el('a');
      a.href = /^https?:/i.test(run.href) ? run.href : '#';
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
      inlineNodes(a, run.children);
      parent.append(a);
    } else {
      const node = el(run.type === 'strong' ? 'strong' : run.type === 'em' ? 'em' : 'del');
      inlineNodes(node, run.children || []);
      parent.append(node);
    }
  }
}

export function mdNodes(parent, list) {
  for (const block of list) {
    if (block.type === 'paragraph') {
      const p = el('p');
      inlineNodes(p, block.inline);
      parent.append(p);
    } else if (block.type === 'heading') {
      const h = el(`h${Math.min(6, block.level + 2)}`);
      inlineNodes(h, block.inline);
      parent.append(h);
    } else if (block.type === 'code') {
      const pre = el('pre', 'code', block.code);
      if (block.lang) pre.dataset.lang = block.lang;
      parent.append(pre);
    } else if (block.type === 'list') {
      const items = el(block.ordered ? 'ol' : 'ul');
      if (block.ordered && block.start !== 1) items.start = block.start;
      for (const item of block.items) {
        const li = el('li');
        mdNodes(li, item);
        items.append(li);
      }
      parent.append(items);
    } else if (block.type === 'quote') {
      const quote = el('blockquote');
      mdNodes(quote, block.blocks);
      parent.append(quote);
    } else if (block.type === 'rule') {
      parent.append(el('hr'));
    } else if (block.type === 'table') {
      const wrap = el('div', 'md-table');
      const table = el('table');
      const cell = (tag, runs, k) => {
        const c = el(tag, block.align[k] ? `align-${block.align[k]}` : '');
        inlineNodes(c, runs);
        return c;
      };
      const head = el('tr');
      head.append(...block.head.map((runs, k) => cell('th', runs, k)));
      const thead = el('thead');
      const tbody = el('tbody');
      thead.append(head);
      for (const row of block.rows) {
        const tr = el('tr');
        tr.append(...row.map((runs, k) => cell('td', runs, k)));
        tbody.append(tr);
      }
      table.append(thead, tbody);
      wrap.append(table);
      parent.append(wrap);
    }
  }
  return parent;
}
