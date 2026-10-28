const colors = new Map([[31,'ansi-red'],[32,'ansi-green'],[33,'ansi-yellow'],[34,'ansi-blue'],[90,'ansi-dim'],[91,'ansi-red'],[92,'ansi-green'],[93,'ansi-yellow'],[94,'ansi-blue']]);
export class Terminal {
  constructor(node) {
    this.node = node;
    this.lines = [[]]; this.column = 0; this.cells = 0; this.color = ''; this.escape = ''; this.mode = '';
    this.decoders = [null,new TextDecoder(),new TextDecoder()]; this.frame = 0;
  }
  write(bytes, stream = 1) {
    const decoder = this.decoders[stream] || this.decoders[1];
    for (const char of decoder.decode(new Uint8Array(bytes), {stream:true})) this.char(char, stream);
    if (!this.frame) this.frame = requestAnimationFrame(() => {this.frame = 0; this.paint();});
  }
  char(char, stream) {
    if (this.mode === 'osc') {
      if (char === '\x07' || (this.escape.endsWith('\x1b') && char === '\\')) {this.mode = ''; this.escape = '';}
      else this.escape = char;
      return;
    }
    if (this.mode === 'esc') {
      this.mode = char === '[' ? 'csi' : char === ']' ? 'osc' : ''; this.escape = ''; return;
    }
    if (this.mode === 'csi') {
      if (char >= '@' && char <= '~') {
        const params = this.escape.split(';').map(Number);
        if (char === 'm') for (const code of params) {if (code === 0 || code === 39) this.color = ''; else if (code === 2) this.color = 'ansi-dim'; else if (colors.has(code)) this.color = colors.get(code);}
        if (char === 'K') {const line = this.lines.at(-1); const before = line.length; if (params[0] === 2) line.length = 0; else line.splice(this.column); this.cells -= before - line.length;}
        this.mode = ''; this.escape = '';
      } else if (this.escape.length < 100) this.escape += char;
      else {this.mode = ''; this.escape = '';}
      return;
    }
    if (char === '\x1b') {this.mode = 'esc'; return;}
    if (char === '\r') {this.column = 0; return;}
    if (char === '\n') {this.lines.push([]); this.column = 0; if (this.lines.length > 800) this.cells -= this.lines.shift().length; return;}
    if (char === '\b') {this.column = Math.max(0,this.column - 1); return;}
    if (char === '\t') {for (let n = 4 - this.column % 4; n > 0; n--) this.char(' ',stream); return;}
    if (char < ' ' || char === '\x7f' || this.column >= 2048) return;
    const line = this.lines.at(-1);
    if (this.column >= line.length) this.cells += this.column - line.length + 1;
    line[this.column++] = [char,this.color || (stream === 2 ? 'stderr' : '')];
    while (this.cells > 65536 && this.lines.length > 1) this.cells -= this.lines.shift().length;
  }
  paint() {
    const follow = this.node.scrollHeight - this.node.scrollTop - this.node.clientHeight < 60;
    const fragment = document.createDocumentFragment();
    let span = null, previous = null;
    for (let i = 0; i < this.lines.length; i++) {
      if (i) fragment.append(document.createTextNode('\n'));
      span = null; previous = null;
      let text = '';
      const flush = () => {if(span && text) span.textContent = text; text = '';};
      for (const cell of this.lines[i]) {
        const [char, style] = cell || [' ',''];
        if (style !== previous) {flush(); span = document.createElement('span'); span.className = style; fragment.append(span); previous = style;}
        text += char;
      }
      flush();
    }
    this.node.replaceChildren(fragment);
    if (follow) this.node.scrollTop = this.node.scrollHeight;
  }
  text() {return this.lines.map(line => line.map(cell => cell?.[0] || ' ').join('')).join('\n');}
}
