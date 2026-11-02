// Files for the document previews' tests (review_browser.mjs): a Word, an Excel and a
// PowerPoint document written as those programs write them (parts, relationships,
// namespaces, styles, numbering, a theme), a PDF, an SVG written without its namespace,
// one whose markup is no XML, and a short WAV. The zips alternate stored and deflated
// parts, so both ways of reading a part are used.
import { crc32, deflateRawSync } from 'node:zlib';

const REL = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';
const PACKAGE = 'http://schemas.openxmlformats.org/package/2006/relationships';
/** A 4x4 PNG. */
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEklEQVR4nGP4z8DwHwyBDAYGAEdJB/mEgnI9AAAAAElFTkSuQmCC', 'base64');

/** `parts` ({name: text or bytes}) as a zip, every other part deflated (and every large
 * one). */
function zipOf(parts) {
  const local = [];
  const central = [];
  let offset = 0;
  Object.entries(parts).forEach(([name, content], at) => {
    const bytes = Buffer.from(content);
    const deflated = at % 2 === 1 || bytes.length > 65536;
    const data = deflated ? deflateRawSync(bytes) : bytes;
    const nameBytes = Buffer.from(name);
    const sum = crc32(bytes);
    const head = Buffer.alloc(30);
    head.writeUInt32LE(0x04034b50, 0);
    head.writeUInt16LE(20, 4);
    head.writeUInt16LE(deflated ? 8 : 0, 8);
    head.writeUInt32LE(sum, 14);
    head.writeUInt32LE(data.length, 18);
    head.writeUInt32LE(bytes.length, 22);
    head.writeUInt16LE(nameBytes.length, 26);
    local.push(head, nameBytes, data);
    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt16LE(deflated ? 8 : 0, 10);
    entry.writeUInt32LE(sum, 16);
    entry.writeUInt32LE(data.length, 20);
    entry.writeUInt32LE(bytes.length, 24);
    entry.writeUInt16LE(nameBytes.length, 28);
    entry.writeUInt32LE(offset, 42);
    central.push(entry, nameBytes);
    offset += 30 + nameBytes.length + data.length;
  });
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(Object.keys(parts).length, 8);
  end.writeUInt16LE(Object.keys(parts).length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return new Uint8Array(Buffer.concat([...local, directory, end]));
}

const rels = (list) => `<?xml version="1.0"?><Relationships xmlns="${PACKAGE}">${list.map(([id, type, target, external]) => `<Relationship Id="${id}" Type="${type.includes('/') ? type : `${REL}/${type}`}" Target="${target}"${external ? ' TargetMode="External"' : ''}/>`).join('')}</Relationships>`;
const root = (main) => rels([['rId1', 'officeDocument', main], ['rId2', `${PACKAGE}/metadata/core-properties`, 'docProps/core.xml']]);
const core = '<?xml version="1.0"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Quarterly &amp; Plan</dc:title></cp:coreProperties>';
const types = '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/></Types>';

const W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"';

function word() {
  const document = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document ${W}><w:body>
<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Quarterly Plan</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Goals &lt;2026&gt;</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">Plain, </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t xml:space="preserve"> and </w:t></w:r><w:r><w:rPr><w:i/><w:color w:val="C00000"/></w:rPr><w:t>red italic</w:t></w:r><w:r><w:t>.</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First step</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Detail</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Second step</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>A bullet</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="4"/></w:numPr></w:pPr><w:r><w:t>Styled item</w:t></w:r></w:p>
<w:p><w:r><w:rPr><w:strike/><w:dstrike w:val="0"/></w:rPr><w:t>struck</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>Third step</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Secret"/></w:pPr><w:r><w:t>secret words</w:t></w:r></w:p>
<w:p><w:r><w:ruby><w:rubyPr/><w:rt><w:r><w:t>kanji</w:t></w:r></w:rt><w:rubyBase><w:r><w:t>漢字</w:t></w:r></w:rubyBase></w:ruby></w:r><w:r><w:rPr><w:highlight w:val="constructor"/></w:rPr><w:t xml:space="preserve"> odd   spaced</w:t></w:r><w:r><w:endnoteReference w:id="1"/></w:r></w:p>
<w:p><w:r><w:drawing><wp:inline><wp:extent cx="952500" cy="476250"/><wp:docPr id="2" name="Picture 2" descr="Half"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:blipFill><a:blip r:embed="rId8"/><a:srcRect b="50000"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rId9"><w:r><w:t>the site</w:t></w:r></w:hyperlink><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t xml:space="preserve"> on page 1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:del><w:r><w:delText>deleted words</w:delText></w:r></w:del><w:r><w:rPr><w:vanish/></w:rPr><w:t>hidden words</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/><w:shd w:fill="D9E2F3"/></w:tcPr><w:p><w:r><w:t>Wide head</w:t></w:r></w:p></w:tc><w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>Tall</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>a1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b1</w:t></w:r></w:p></w:tc><w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc></w:tr></w:tbl>
<w:p><w:r><w:drawing><wp:inline><wp:extent cx="952500" cy="952500"/><wp:docPr id="1" name="Picture 1" descr="A square"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:blipFill><a:blip r:embed="rId8"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>
<w:sectPr/></w:body></w:document>`;
  const styles = `<?xml version="1.0"?><w:styles ${W}><w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Secret"><w:name w:val="Secret"/><w:rPr><w:vanish/></w:rPr></w:style><w:style w:type="numbering" w:styleId="Steps"><w:name w:val="Steps"/><w:pPr><w:numPr><w:numId w:val="3"/></w:numPr></w:pPr></w:style></w:styles>`;
  const numbering = `<?xml version="1.0"?><w:numbering ${W}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1.%2)"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="\uf0b7"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="2"><w:numStyleLink w:val="Steps"/></w:abstractNum><w:abstractNum w:abstractNumId="3"><w:styleLink w:val="Steps"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1)"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num><w:num w:numId="3"><w:abstractNumId w:val="3"/></w:num><w:num w:numId="4"><w:abstractNumId w:val="2"/></w:num><w:num w:numId="5"><w:abstractNumId w:val="0"/></w:num></w:numbering>`;
  const endnotes = `<?xml version="1.0"?><w:endnotes ${W}><w:endnote w:id="1"><w:p><w:r><w:t xml:space="preserve"> An endnote.</w:t></w:r></w:p></w:endnote></w:endnotes>`;
  const footnotes = `<?xml version="1.0"?><w:footnotes ${W}><w:footnote w:type="separator" w:id="-1"><w:p/></w:footnote><w:footnote w:id="2"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> A footnote.</w:t></w:r></w:p></w:footnote></w:footnotes>`;
  return zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('word/document.xml'), 'docProps/core.xml': core,
    'word/document.xml': document, 'word/styles.xml': styles, 'word/numbering.xml': numbering, 'word/footnotes.xml': footnotes, 'word/endnotes.xml': endnotes,
    'word/_rels/document.xml.rels': rels([['rId1', 'styles', 'styles.xml'], ['rId2', 'numbering', 'numbering.xml'], ['rId3', 'footnotes', 'footnotes.xml'], ['rId4', 'endnotes', 'endnotes.xml'],
      ['rId8', 'image', 'media/image1.png'], ['rId9', 'hyperlink', 'https://example.com/a?b=1&amp;c=2', true]]),
    'word/media/image1.png': PNG,
  });
}

const S = 'xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"';

function excel() {
  return zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('xl/workbook.xml'), 'docProps/core.xml': core,
    'xl/workbook.xml': `<?xml version="1.0"?><workbook ${S}><workbookPr/><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Secret" sheetId="2" state="veryHidden" r:id="rId2"/><sheet name="Notes" sheetId="3" state="hidden" r:id="rId3"/><sheet name="Huge" sheetId="4" r:id="rId6"/></sheets></workbook>`,
    'xl/_rels/workbook.xml.rels': rels([['rId1', 'worksheet', 'worksheets/sheet1.xml'], ['rId2', 'worksheet', 'worksheets/sheet2.xml'], ['rId3', 'worksheet', '/xl/worksheets/sheet3.xml'], ['rId4', 'sharedStrings', 'sharedStrings.xml'], ['rId5', 'styles', 'styles.xml'], ['rId6', 'worksheet', 'worksheets/sheet4.xml']]),
    'xl/sharedStrings.xml': `<?xml version="1.0"?><sst ${S}><si><t>Item</t></si><si><t>Amount</t></si><si><r><t>Rent </t></r><r><rPr><b/></rPr><t>&amp; power</t></r></si><si><t>Due</t></si><si><t>hush</t></si><si><t>Line_x000A_two</t></si></sst>`,
    'xl/styles.xml': `<?xml version="1.0"?><styleSheet ${S}><numFmts count="10"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/><numFmt numFmtId="165" formatCode="0.0,,&quot;M&quot;"/><numFmt numFmtId="166" formatCode="#,##0.00 [$€-407]"/><numFmt numFmtId="167" formatCode="[h]:mm:ss"/><numFmt numFmtId="168" formatCode="00000"/><numFmt numFmtId="169" formatCode="000-00-0000"/><numFmt numFmtId="170" formatCode="#,##0.00&quot;р.&quot;"/><numFmt numFmtId="171" formatCode="&quot;Rs.&quot; #,##0"/><numFmt numFmtId="172" formatCode="[&gt;=1000000]0.0,,&quot;M&quot;;[&gt;=1000]0.0,&quot;K&quot;;0"/><numFmt numFmtId="173" formatCode=";;;"/><numFmt numFmtId="174" formatCode="[s]"/></numFmts><cellXfs count="16"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="14"/><xf numFmtId="10"/><xf numFmtId="12"/><xf numFmtId="11"/><xf numFmtId="165"/><xf numFmtId="166"/><xf numFmtId="167"/><xf numFmtId="168"/><xf numFmtId="169"/><xf numFmtId="170"/><xf numFmtId="171"/><xf numFmtId="172"/><xf numFmtId="173"/><xf numFmtId="174"/></cellXfs></styleSheet>`,
    'xl/worksheets/sheet1.xml': `<?xml version="1.0"?><worksheet ${S}><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>3</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2" s="1"><v>1234.5</v></c><c r="C2" s="2"><v>46023</v></c><c r="D2" s="3"><v>0.125</v></c></row><row r="4"><c r="A4" t="inlineStr"><is><t>Total</t></is></c><c r="B4" s="1"><f>B2</f><v>1234.5</v></c><c r="C4" t="b"><v>1</v></c><c r="D4"><v>0.30000000000000004</v></c></row><row r="5"><c r="A5" s="4"><v>1.5</v></c><c r="B5" s="5"><v>1234</v></c></row><row r="6"><c r="A6" s="6"><v>1500000</v></c><c r="B6" s="7"><v>1234.5</v></c><c r="C6" s="8"><v>1.5</v></c><c r="D6"><f>SUM(B2:B4)</f><v></v></c><c r="E6" t="s"><v>5</v></c></row><row r="7"><c r="A7" s="9"><v>2134</v></c><c r="B7" s="10"><v>123456789</v></c><c r="C7" s="11"><v>1234.5</v></c><c r="D7" s="12"><v>1234</v></c><c r="E7" s="13"><v>500</v></c></row><row r="8"><c r="A8" s="13"><v>1500</v></c><c r="B8" s="14"><v>42</v></c><c r="C8" t="str"><f>IF(A1="","","x")</f><v></v></c><c r="D8" t="d" s="2"><v>2026-03-04T05:06:07</v></c><c r="E8" s="2"><v>60</v></c></row><row r="9"><c r="A9" s="15"><v>0.0010416666666666667</v></c><c r="B9" s="13"><v>2500000</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A3:D3"/></mergeCells></worksheet>`,
    'xl/worksheets/sheet2.xml': `<?xml version="1.0"?><worksheet ${S}><sheetData><row r="1"><c r="A1" t="s"><v>4</v></c></row></sheetData></worksheet>`,
    'xl/worksheets/sheet3.xml': `<?xml version="1.0"?><worksheet ${S}><sheetData><row r="1"><c r="B1" t="str"><v>formula text</v></c></row></sheetData></worksheet>`,
    // Over what a preview parses (16 MiB), deflated to almost nothing.
    'xl/worksheets/sheet4.xml': `<?xml version="1.0"?><worksheet ${S}><sheetData>${' '.repeat(17 * 1024 * 1024)}</sheetData></worksheet>`,
  });
}

const P = 'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"';
const placeholder = (id, type, body, extra = '') => `<p:sp><p:nvSpPr><p:cNvPr id="${id}" name="${id}"/><p:cNvSpPr/><p:nvPr><p:ph${type}/></p:nvPr></p:nvSpPr><p:spPr>${extra}</p:spPr>${body}</p:sp>`;

function powerPoint() {
  const master = `<?xml version="1.0"?><p:sldMaster ${P}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree>`
    + placeholder(2, ' type="title"', '', '<a:xfrm><a:off x="838200" y="365125"/><a:ext cx="10515600" cy="1325563"/></a:xfrm>')
    + placeholder(3, ' type="body" idx="1"', '', '<a:xfrm><a:off x="838200" y="1825625"/><a:ext cx="10515600" cy="4351338"/></a:xfrm>')
    + '</p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1"/><p:txStyles>'
    + '<p:titleStyle><a:lvl1pPr><a:defRPr sz="4400"><a:solidFill><a:schemeClr val="tx2"/></a:solidFill></a:defRPr></a:lvl1pPr></p:titleStyle>'
    + '<p:bodyStyle><a:lvl1pPr><a:buChar char="•"/><a:defRPr sz="2800"/></a:lvl1pPr><a:lvl2pPr><a:buChar char="–"/><a:defRPr sz="2400"/></a:lvl2pPr></p:bodyStyle>'
    + '<p:otherStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>';
  const theme = `<?xml version="1.0"?><a:theme ${P} name="T"><a:themeElements><a:clrScheme name="c"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="0E2841"/></a:dk2><a:lt2><a:srgbClr val="E8E8E8"/></a:lt2><a:accent1><a:srgbClr val="156082"/></a:accent1></a:clrScheme></a:themeElements></a:theme>`;
  const titleLayout = `<?xml version="1.0"?><p:sldLayout ${P}><p:cSld><p:spTree>`
    + placeholder(2, ' type="ctrTitle"', '<p:txBody><a:bodyPr anchor="b"/><a:lstStyle><a:lvl1pPr algn="ctr"><a:defRPr sz="6000"/></a:lvl1pPr></a:lstStyle><a:p/></p:txBody>', '<a:xfrm><a:off x="1524000" y="1122363"/><a:ext cx="9144000" cy="2387600"/></a:xfrm>')
    + placeholder(3, ' type="subTitle" idx="1"', '<p:txBody><a:bodyPr/><a:lstStyle><a:lvl1pPr algn="ctr"><a:buNone/><a:defRPr sz="2400"/></a:lvl1pPr></a:lstStyle><a:p/></p:txBody>', '<a:xfrm><a:off x="1524000" y="3602038"/><a:ext cx="9144000" cy="1655762"/></a:xfrm>')
    + '</p:spTree></p:cSld></p:sldLayout>';
  const text = (paragraphs) => `<p:txBody><a:bodyPr/>${paragraphs}</p:txBody>`;
  const first = `<?xml version="1.0"?><p:sld ${P}><p:cSld><p:spTree>`
    + placeholder(2, ' type="ctrTitle"', text('<a:p><a:r><a:t>Launch &lt;Day&gt;</a:t></a:r></a:p>'))
    + placeholder(3, ' type="subTitle" idx="1"', text('<a:p><a:r><a:t>A subtitle</a:t></a:r></a:p>'))
    + '</p:spTree></p:cSld></p:sld>';
  const second = `<?xml version="1.0"?><p:sld ${P}><p:cSld><p:bg><p:bgPr><a:solidFill><a:schemeClr val="accent1"><a:lumMod val="20000"/><a:lumOff val="80000"/></a:schemeClr></a:solidFill></p:bgPr></p:bg><p:spTree>`
    + placeholder(2, ' type="title"', text('<a:p><a:r><a:t>Agenda</a:t></a:r></a:p>'))
    + placeholder(3, ' idx="1"', text('<a:p><a:r><a:t>Point one</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:rPr b="1"/><a:t>Sub point</a:t></a:r></a:p>'
      + '<a:p><a:r><a:rPr sz="3600"/><a:t>Big</a:t></a:r><a:r><a:t> then normal</a:t></a:r></a:p>'))
    // A picture placeholder placed by its layout.
    + '<p:pic><p:nvPicPr><p:cNvPr id="6" name="Photo" descr="placed"/><p:cNvPicPr/><p:nvPr><p:ph type="pic" idx="2"/></p:nvPr></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr/></p:pic>'
    + '<p:pic><p:nvPicPr><p:cNvPr id="4" name="P" descr="logo"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr><a:xfrm><a:off x="10000000" y="5000000"/><a:ext cx="1000000" cy="1000000"/></a:xfrm></p:spPr></p:pic>'
    + '<p:sp><p:nvSpPr><p:cNvPr id="8" name="Pink"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="6000000"/><a:ext cx="1219200" cy="685800"/></a:xfrm><a:solidFill><a:srgbClr val="FF0080"><a:lumMod val="50000"/></a:srgbClr></a:solidFill></p:spPr></p:sp>'
    + '<p:sp><p:nvSpPr><p:cNvPr id="9" name="Veil"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="1219200" y="6000000"/><a:ext cx="1219200" cy="685800"/></a:xfrm><a:solidFill><a:srgbClr val="000000"><a:alpha val="40000"/></a:srgbClr></a:solidFill></p:spPr></p:sp>'
    + '<p:pic><p:nvPicPr><p:cNvPr id="10" name="Cut" descr="cut"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:srcRect l="25000" r="25000"/></p:blipFill><p:spPr><a:xfrm><a:off x="3657600" y="6000000"/><a:ext cx="609600" cy="685800"/></a:xfrm></p:spPr></p:pic>'
    + '<p:sp><p:nvSpPr><p:cNvPr id="5" name="Gone" hidden="1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="100" cy="100"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>invisible shape</a:t></a:r></a:p></p:txBody></p:sp>'
    + '</p:spTree></p:cSld></p:sld>';
  const toMaster = rels([['rId1', 'slideMaster', '../slideMasters/slideMaster1.xml']]);
  return zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('ppt/presentation.xml'), 'docProps/core.xml': core,
    'ppt/presentation.xml': `<?xml version="1.0"?><p:presentation ${P}><p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId3"/></p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/></p:presentation>`,
    'ppt/_rels/presentation.xml.rels': rels([['rId2', 'slide', 'slides/slide1.xml'], ['rId3', 'slide', 'slides/slide2.xml']]),
    'ppt/theme/theme1.xml': theme,
    'ppt/slideMasters/slideMaster1.xml': master,
    'ppt/slideMasters/_rels/slideMaster1.xml.rels': rels([['rId1', 'theme', '../theme/theme1.xml']]),
    'ppt/slideLayouts/slideLayout1.xml': titleLayout,
    'ppt/slideLayouts/_rels/slideLayout1.xml.rels': toMaster,
    'ppt/slideLayouts/slideLayout2.xml': `<?xml version="1.0"?><p:sldLayout ${P}><p:cSld><p:spTree>`
      + placeholder(7, ' type="pic" idx="2"', '', '<a:xfrm><a:off x="6096000" y="1825625"/><a:ext cx="5257800" cy="4351338"/></a:xfrm>')
      + '</p:spTree></p:cSld></p:sldLayout>',
    'ppt/slideLayouts/_rels/slideLayout2.xml.rels': toMaster,
    'ppt/slides/slide1.xml': first,
    'ppt/slides/slide2.xml': second,
    'ppt/slides/_rels/slide1.xml.rels': rels([['rId1', 'slideLayout', '../slideLayouts/slideLayout1.xml']]),
    'ppt/slides/_rels/slide2.xml.rels': rels([['rId1', 'slideLayout', '../slideLayouts/slideLayout2.xml'], ['rId2', 'image', '../media/image1.png']]),
    'ppt/media/image1.png': PNG,
  });
}

/** A one-page PDF saying `text`. */
function pdf(text) {
  const stream = `BT /F1 36 Tf 72 700 Td (${text}) Tj ET`;
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>', '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
    `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`, '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let out = '%PDF-1.4\n';
  const offsets = objects.map((object, at) => {
    const offset = out.length;
    out += `${at + 1} 0 obj\n${object}\nendobj\n`;
    return offset;
  });
  const xref = out.length;
  out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n${offsets.map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('')}`;
  out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return new TextEncoder().encode(out);
}

/** A quarter second of a tone, 8 kHz mono 16-bit. */
function wav() {
  const samples = 2000;
  const out = Buffer.alloc(44 + samples * 2);
  out.write('RIFF', 0);
  out.writeUInt32LE(36 + samples * 2, 4);
  out.write('WAVEfmt ', 8);
  out.writeUInt32LE(16, 16);
  out.writeUInt16LE(1, 20);
  out.writeUInt16LE(1, 22);
  out.writeUInt32LE(8000, 24);
  out.writeUInt32LE(16000, 28);
  out.writeUInt16LE(2, 32);
  out.writeUInt16LE(16, 34);
  out.write('data', 36);
  out.writeUInt32LE(samples * 2, 40);
  for (let n = 0; n < samples; n++) out.writeInt16LE(Math.round(8000 * Math.sin((2 * Math.PI * 440 * n) / 8000)), 44 + n * 2);
  return new Uint8Array(out);
}

/** Documents whose numbers are out of every bound (a list level of a hundred million, a
 * format of 200 decimals and 30 thousands' commas, a slide level below zero, a span of a
 * billion): each still draws, at once, with its numbers bounded. */
function hostile() {
  const numbering = `<?xml version="1.0"?><w:numbering ${W}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="100000000"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="0"><w:start w:val="999999999"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1)"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="-3"><w:startOverride w:val="5"/></w:lvlOverride></w:num></w:numbering>`;
  const document = `<?xml version="1.0"?><w:document ${W}><w:body><w:p><w:pPr><w:outlineLvl w:val="-2"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Bounded item</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val="1000000000"/></w:tcPr><w:p><w:r><w:t>wide</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>`;
  const docx = zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('word/document.xml'), 'word/document.xml': document, 'word/numbering.xml': numbering,
    'word/_rels/document.xml.rels': rels([['rId2', 'numbering', 'numbering.xml']]),
  });
  const xlsx = zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('xl/workbook.xml'),
    'xl/workbook.xml': `<?xml version="1.0"?><workbook ${S}><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>`,
    'xl/_rels/workbook.xml.rels': rels([['rId1', 'worksheet', 'worksheets/sheet1.xml'], ['rId2', 'styles', 'styles.xml']]),
    'xl/styles.xml': `<?xml version="1.0"?><styleSheet ${S}><numFmts count="2"><numFmt numFmtId="164" formatCode="0.${'0'.repeat(200)}E+00"/><numFmt numFmtId="165" formatCode="0${','.repeat(30)}"/></numFmts><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/></cellXfs></styleSheet>`,
    'xl/worksheets/sheet1.xml': `<?xml version="1.0"?><worksheet ${S}><sheetData><row r="1"><c r="A1" s="1"><v>1234</v></c><c r="B1" s="2"><v>5</v></c></row></sheetData></worksheet>`,
  });
  const pptx = zipOf({
    '[Content_Types].xml': types, '_rels/.rels': root('ppt/presentation.xml'),
    'ppt/presentation.xml': `<?xml version="1.0"?><p:presentation ${P}><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/></p:presentation>`,
    'ppt/_rels/presentation.xml.rels': rels([['rId2', 'slide', 'slides/slide1.xml']]),
    'ppt/slides/slide1.xml': `<?xml version="1.0"?><p:sld ${P}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="T"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="12192000" cy="6858000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:pPr lvl="-5"><a:buAutoNum type="arabicPeriod" startAt="99999999"/></a:pPr><a:r><a:t>Low level</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>`,
  });
  return { 'hostile.docx': docx, 'hostile.xlsx': xlsx, 'hostile.pptx': pptx };
}

export function officeFixtures() {
  const text = (value) => new TextEncoder().encode(value);
  return {
    'plan.docx': word(),
    'budget.xlsx': excel(),
    'deck.pptx': powerPoint(),
    'report.pdf': pdf('Hello from a PDF'),
    // As models often write SVG for a page: no namespace (a picture reads it as XML).
    'drawing.svg': text('<svg width="120" height="80" viewBox="0 0 120 80"><rect width="120" height="80" fill="#2a7"/><use xlink:href="#none"/></svg>'),
    // An HTML entity and a stray ampersand: no XML, but a browser draws it inside HTML.
    'loose.svg': text('<svg width="200" height="60"><text x="10" y="35">Fish&nbsp;& chips</text></svg>'),
    'tone.wav': wav(),
  };
}

export const hostileFixtures = hostile;
