const encoder = new TextEncoder();
const OFL_1_1_LICENSE = `Copyright 2026 rxls contributors

SIL OPEN FONT LICENSE

Version 1.1 - 26 February 2007

PREAMBLE

The goals of the Open Font License (OFL) are to stimulate worldwide development of collaborative font projects, to support the font creation efforts of academic and linguistic communities, and to provide a free and open framework in which fonts may be shared and improved in partnership with others.

The OFL allows the licensed fonts to be used, studied, modified and redistributed freely as long as they are not sold by themselves. The fonts, including any derivative works, can be bundled, embedded, redistributed and/or sold with any software provided that any reserved names are not used by derivative works. The fonts and derivatives, however, cannot be released under any other type of license. The requirement for fonts to remain under this license does not apply to any document created using the fonts or their derivatives.

DEFINITIONS

"Font Software" refers to the set of files released by the Copyright Holder(s) under this license and clearly marked as such. This may include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the copyright statement(s).

"Original Version" refers to the collection of Font Software components as distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting, or substituting — in part or in whole — any of the components of the Original Version, by changing formats or by porting the Font Software to a new environment.

"Author" refers to any designer, engineer, programmer, technical writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS

Permission is hereby granted, free of charge, to any person obtaining a copy of the Font Software, to use, study, copy, merge, embed, modify, redistribute, and sell modified and unmodified copies of the Font Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components, in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled, redistributed and/or sold with any software, provided that each copy contains the above copyright notice and this license. These can be included either as stand-alone text files, human-readable headers or in the appropriate machine-readable metadata fields within text or binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font Name(s) unless explicit written permission is granted by the corresponding Copyright Holder. This restriction only applies to the primary font name as presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font Software shall not be used to promote, endorse or advertise any Modified Version, except to acknowledge the contribution(s) of the Copyright Holder(s) and the Author(s) or with their explicit written permission.

5) The Font Software, modified or unmodified, in part or in whole, must be distributed entirely under this license, and must not be distributed under any other license. The requirement for fonts to remain under this license does not apply to any document created using the Font Software.

TERMINATION

This license becomes null and void if any of the above conditions are not met.

DISCLAIMER

THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM OTHER DEALINGS IN THE FONT SOFTWARE.
`;

export const FIXTURE_ROWS = 128;
export const FIXTURE_COLUMNS = 64;
export const FIXTURE_TILE = Object.freeze({
  firstRow: 0,
  firstCol: 0,
  lastRow: 63,
  lastCol: 31
});
export const FIXTURE_TILE_PAINT_CELLS =
  (FIXTURE_TILE.lastRow - FIXTURE_TILE.firstRow + 1) *
  (FIXTURE_TILE.lastCol - FIXTURE_TILE.firstCol + 1);
export const FIXTURE_TILE_MEASURED_CELLS =
  (FIXTURE_TILE.lastRow - FIXTURE_TILE.firstRow + 1) * FIXTURE_COLUMNS;

export const BROWSER_FIXTURE_PROVENANCE = Object.freeze({
  schema: "rxls.render-browser-fixture.v1",
  generator: "bindings/render-wasm/tests/browser/fixture.mjs",
  ownership: "project-authored synthetic workbook, image, and font",
  inputs: Object.freeze({
    workbook: "deterministic stored-ZIP OOXML generated without office software",
    image: "deterministic 64x64 RGBA PNG generated from integer coordinates",
    font:
      "minimal OpenType tables mirrored from the project-owned render/src/font.rs test generator",
    license:
      "complete OFL-1.1 legal text bundled locally with the project copyright notice; no runtime fetch"
  }),
  externalAssets: false,
  rows: FIXTURE_ROWS,
  columns: FIXTURE_COLUMNS,
  fontFamily: "RXLS Fixture Sans"
});

export async function createBrowserFixture() {
  return buildFixture();
}

async function buildFixture() {
  const font = syntheticFont(BROWSER_FIXTURE_PROVENANCE.fontFamily, [
    [0x20, 0x20, 2],
    [0x21, 0x7e, 1],
    [0xac00, 0xd7a3, 1]
  ]);
  const imageWidth = 64;
  const imageHeight = 64;
  const decodedImage = syntheticRgba(imageWidth, imageHeight);
  const image = syntheticPng(imageWidth, imageHeight, decodedImage);
  const fontPack = await syntheticFontPack(font);
  const workbook = syntheticWorkbook(image);
  const metadata = {
    workbookSha256: await sha256Hex(workbook),
    workbookBytes: workbook.byteLength,
    imageSha256: await sha256Hex(image),
    imageBytes: image.byteLength,
    imageWidth,
    imageHeight,
    decodedImageSha256: await sha256Hex(decodedImage),
    decodedImageBytes: decodedImage.byteLength,
    renderedImageSha256:
      "b848eb79a6b54cefd9772661737bed9d9273a48df8ee3082cb70023f1b7c8530",
    renderedImageBytes: 13_029,
    fontSha256: await sha256Hex(font),
    fontBytes: font.byteLength,
    fontPackSha256: JSON.parse(new TextDecoder().decode(fontPack.manifest)).pack_sha256,
    rows: FIXTURE_ROWS,
    columns: FIXTURE_COLUMNS,
    cells: FIXTURE_ROWS * FIXTURE_COLUMNS,
    tilePaintCells: FIXTURE_TILE_PAINT_CELLS,
    tileMeasuredCells: FIXTURE_TILE_MEASURED_CELLS
  };
  return { workbook, fontPack, metadata };
}

async function syntheticFontPack(font) {
  const license = encoder.encode(OFL_1_1_LICENSE);
  const config = encoder.encode("<fontconfig><dir>fonts</dir></fontconfig>\n");
  const fonts = [
    {
      bytes: font.byteLength,
      family: BROWSER_FIXTURE_PROVENANCE.fontFamily,
      output: "fonts/RxlsFixtureSans.ttf",
      sha256: await sha256Hex(font),
      style: "normal",
      weight: 400
    }
  ];
  const licenses = [
    {
      bytes: license.byteLength,
      output: "licenses/OFL.txt",
      sha256: await sha256Hex(license)
    }
  ];
  const fontsConfSha256 = await sha256Hex(config);
  const identity = {
    fonts,
    fonts_conf_sha256: fontsConfSha256,
    licenses
  };
  const canonical = `${JSON.stringify(identity, null, 2)}\n`;
  const manifest = {
    schema: "rxls.render-font-pack.v1",
    license: "SIL-OFL-1.1",
    fonts,
    licenses,
    fonts_conf_sha256: fontsConfSha256,
    total_bytes: font.byteLength + license.byteLength + config.byteLength,
    pack_sha256: await sha256Hex(encoder.encode(canonical))
  };
  return {
    manifest: encoder.encode(`${JSON.stringify(manifest, null, 2)}\n`),
    members: [
      { name: "fonts/RxlsFixtureSans.ttf", bytes: font },
      { name: "licenses/OFL.txt", bytes: license },
      { name: "fonts.conf", bytes: config }
    ]
  };
}

function syntheticWorkbook(image) {
  const lastCell = `${columnName(FIXTURE_COLUMNS - 1)}${FIXTURE_ROWS}`;
  const rows = [];
  for (let row = 0; row < FIXTURE_ROWS; row += 1) {
    const cells = [];
    for (let column = 0; column < FIXTURE_COLUMNS; column += 1) {
      const reference = `${columnName(column)}${row + 1}`;
      const value = `RXLS R${String(row).padStart(3, "0")} C${String(column).padStart(
        2,
        "0"
      )}`;
      cells.push(
        `<c r="${reference}" s="1" t="inlineStr"><is><t>${value}</t></is></c>`
      );
    }
    rows.push(`<row r="${row + 1}">${cells.join("")}</row>`);
  }

  const contentTypes =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
    '<Default Extension="xml" ContentType="application/xml"/>' +
    '<Default Extension="png" ContentType="image/png"/>' +
    '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
    '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>' +
    '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>' +
    '<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>' +
    "</Types>";
  const packageRelationships =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
    '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>' +
    "</Relationships>";
  const workbook =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" ' +
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">' +
    '<sheets><sheet name="Browser stress" sheetId="1" r:id="rId1"/></sheets>' +
    "</workbook>";
  const workbookRelationships =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
    '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>' +
    '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>' +
    "</Relationships>";
  const styles =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
    '<fonts count="2">' +
    '<font><sz val="11"/><name val="Calibri"/></font>' +
    `<font><sz val="11"/><name val="${BROWSER_FIXTURE_PROVENANCE.fontFamily}"/></font>` +
    "</fonts>" +
    '<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>' +
    '<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>' +
    '<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>' +
    '<cellXfs count="2">' +
    '<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>' +
    '<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/>' +
    "</cellXfs>" +
    '<cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>' +
    "</styleSheet>";
  const worksheet =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" ' +
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">' +
    `<dimension ref="A1:${lastCell}"/>` +
    '<sheetFormatPr defaultRowHeight="15"/>' +
    `<cols><col min="1" max="${FIXTURE_COLUMNS}" width="12" customWidth="1"/></cols>` +
    `<sheetData>${rows.join("")}</sheetData>` +
    '<pageMargins left="0.25" right="0.25" top="0.5" bottom="0.5" header="0.2" footer="0.2"/>' +
    '<pageSetup paperSize="9" orientation="landscape"/>' +
    '<drawing r:id="rIdDrawing"/>' +
    "</worksheet>";
  const worksheetRelationships =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
    '<Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>' +
    "</Relationships>";
  const drawing =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" ' +
    'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" ' +
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">' +
    '<xdr:twoCellAnchor editAs="oneCell">' +
    '<xdr:from><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>' +
    '<xdr:to><xdr:col>10</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>9</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>' +
    '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="RXLS fixture image" descr="Project-authored deterministic pixels"/>' +
    '<xdr:cNvPicPr><a:picLocks noChangeAspect="1"/></xdr:cNvPicPr></xdr:nvPicPr>' +
    '<xdr:blipFill><a:blip r:embed="rIdImage"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>' +
    '<xdr:spPr><a:xfrm/><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>' +
    "<xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>";
  const drawingRelationships =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
    '<Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>' +
    "</Relationships>";

  return storedZip([
    ["[Content_Types].xml", encoder.encode(contentTypes)],
    ["_rels/.rels", encoder.encode(packageRelationships)],
    ["xl/workbook.xml", encoder.encode(workbook)],
    ["xl/_rels/workbook.xml.rels", encoder.encode(workbookRelationships)],
    ["xl/styles.xml", encoder.encode(styles)],
    ["xl/worksheets/sheet1.xml", encoder.encode(worksheet)],
    ["xl/worksheets/_rels/sheet1.xml.rels", encoder.encode(worksheetRelationships)],
    ["xl/drawings/drawing1.xml", encoder.encode(drawing)],
    ["xl/drawings/_rels/drawing1.xml.rels", encoder.encode(drawingRelationships)],
    ["xl/media/image1.png", image]
  ]);
}

function storedZip(entries) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const nameBytes = encoder.encode(name);
    const checksum = crc32(data);
    const localHeader = concatBytes(
      littleU32(0x04034b50),
      littleU16(20),
      littleU16(0x0800),
      littleU16(0),
      littleU16(0),
      littleU16(0x0021),
      littleU32(checksum),
      littleU32(data.byteLength),
      littleU32(data.byteLength),
      littleU16(nameBytes.byteLength),
      littleU16(0),
      nameBytes,
      data
    );
    local.push(localHeader);
    central.push(
      concatBytes(
        littleU32(0x02014b50),
        littleU16(0x0314),
        littleU16(20),
        littleU16(0x0800),
        littleU16(0),
        littleU16(0),
        littleU16(0x0021),
        littleU32(checksum),
        littleU32(data.byteLength),
        littleU32(data.byteLength),
        littleU16(nameBytes.byteLength),
        littleU16(0),
        littleU16(0),
        littleU16(0),
        littleU16(0),
        littleU32(0),
        littleU32(offset),
        nameBytes
      )
    );
    offset += localHeader.byteLength;
  }
  const centralBytes = concatBytes(...central);
  return concatBytes(
    ...local,
    centralBytes,
    littleU32(0x06054b50),
    littleU16(0),
    littleU16(0),
    littleU16(entries.length),
    littleU16(entries.length),
    littleU32(centralBytes.byteLength),
    littleU32(offset),
    littleU16(0)
  );
}

function syntheticRgba(width, height) {
  const rgba = new Uint8Array(width * height * 4);
  let cursor = 0;
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      rgba[cursor++] = (x * 17 + y * 3) & 0xff;
      rgba[cursor++] = (x * 5 + y * 19) & 0xff;
      rgba[cursor++] = (x * 11 + y * 7) & 0xff;
      rgba[cursor++] = 255;
    }
  }
  return rgba;
}

function syntheticPng(width, height, rgba) {
  if (rgba.byteLength !== width * height * 4) {
    throw new Error("synthetic PNG RGBA length mismatch");
  }
  const scanlines = new Uint8Array(height * (1 + width * 4));
  let cursor = 0;
  let pixel = 0;
  for (let y = 0; y < height; y += 1) {
    scanlines[cursor++] = 0;
    scanlines.set(rgba.subarray(pixel, pixel + width * 4), cursor);
    cursor += width * 4;
    pixel += width * 4;
  }
  const ihdr = concatBytes(
    bigU32(width),
    bigU32(height),
    Uint8Array.of(8, 6, 0, 0, 0)
  );
  return concatBytes(
    Uint8Array.of(137, 80, 78, 71, 13, 10, 26, 10),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", storedZlib(scanlines)),
    pngChunk("IEND", new Uint8Array())
  );
}

function pngChunk(type, data) {
  const typeBytes = encoder.encode(type);
  const body = concatBytes(typeBytes, data);
  return concatBytes(bigU32(data.byteLength), body, bigU32(crc32(body)));
}

function storedZlib(bytes) {
  const blocks = [Uint8Array.of(0x78, 0x01)];
  for (let offset = 0; offset < bytes.byteLength; offset += 65_535) {
    const block = bytes.subarray(offset, Math.min(bytes.byteLength, offset + 65_535));
    const final = offset + block.byteLength === bytes.byteLength;
    blocks.push(
      Uint8Array.of(final ? 1 : 0),
      littleU16(block.byteLength),
      littleU16(0xffff ^ block.byteLength),
      block
    );
  }
  blocks.push(bigU32(adler32(bytes)));
  return concatBytes(...blocks);
}

function syntheticFont(family, groups) {
  const tables = [
    ["cmap", syntheticCmap(groups)],
    ["glyf", syntheticGlyf()],
    ["head", syntheticHead()],
    ["hhea", syntheticHhea()],
    ["hmtx", syntheticHmtx()],
    ["loca", syntheticLoca()],
    ["maxp", syntheticMaxp()],
    ["name", syntheticName(family)],
    ["post", syntheticPost()]
  ].sort(([left], [right]) => left.localeCompare(right));
  const directoryBytes = 12 + tables.length * 16;
  let offset = directoryBytes;
  const records = [];
  for (const [tag, bytes] of tables) {
    records.push([tag, offset, bytes.byteLength]);
    offset += (bytes.byteLength + 3) & ~3;
  }
  const output = [];
  pushU32(output, 0x00010000);
  pushU16(output, tables.length);
  pushU16(output, 0);
  pushU16(output, 0);
  pushU16(output, 0);
  for (const [tag, tableOffset, length] of records) {
    output.push(...encoder.encode(tag));
    pushU32(output, 0);
    pushU32(output, tableOffset);
    pushU32(output, length);
  }
  for (const [, bytes] of tables) {
    output.push(...bytes);
    while (output.length % 4 !== 0) {
      output.push(0);
    }
  }
  return Uint8Array.from(output);
}

function syntheticName(family) {
  const encoded = [];
  for (const character of family) {
    const code = character.codePointAt(0);
    if (code > 0xffff) {
      throw new Error("synthetic font family must be BMP text");
    }
    pushU16(encoded, code);
  }
  const output = [];
  pushU16(output, 0);
  pushU16(output, 1);
  pushU16(output, 18);
  pushU16(output, 3);
  pushU16(output, 1);
  pushU16(output, 0x0409);
  pushU16(output, 1);
  pushU16(output, encoded.length);
  pushU16(output, 0);
  output.push(...encoded);
  return Uint8Array.from(output);
}

function syntheticCmap(groups) {
  const length = 16 + groups.length * 12;
  const output = [];
  pushU16(output, 0);
  pushU16(output, 1);
  pushU16(output, 0);
  pushU16(output, 6);
  pushU32(output, 12);
  pushU16(output, 13);
  pushU16(output, 0);
  pushU32(output, length);
  pushU32(output, 0);
  pushU32(output, groups.length);
  for (const [start, end, glyph] of groups) {
    pushU32(output, start);
    pushU32(output, end);
    pushU32(output, glyph);
  }
  return Uint8Array.from(output);
}

function syntheticGlyf() {
  const glyph = syntheticRectangleGlyph();
  return concatBytes(glyph, glyph);
}

function syntheticRectangleGlyph() {
  const output = [];
  pushI16(output, 1);
  pushI16(output, 0);
  pushI16(output, 0);
  pushI16(output, 500);
  pushI16(output, 700);
  pushU16(output, 3);
  pushU16(output, 0);
  output.push(1, 1, 1, 1);
  for (const value of [0, 500, 0, -500]) {
    pushI16(output, value);
  }
  for (const value of [0, 0, 700, 0]) {
    pushI16(output, value);
  }
  return Uint8Array.from(output);
}

function syntheticHead() {
  const output = [];
  pushU32(output, 0x00010000);
  pushU32(output, 0x00010000);
  pushU32(output, 0);
  pushU32(output, 0x5f0f3cf5);
  pushU16(output, 0);
  pushU16(output, 1000);
  output.push(...new Uint8Array(16));
  for (const value of [0, 0, 500, 700]) {
    pushI16(output, value);
  }
  pushU16(output, 0);
  pushU16(output, 8);
  pushI16(output, 2);
  pushU16(output, 1);
  pushI16(output, 0);
  return Uint8Array.from(output);
}

function syntheticHhea() {
  const output = [];
  pushU32(output, 0x00010000);
  pushI16(output, 800);
  pushI16(output, -200);
  pushI16(output, 200);
  output.push(...new Uint8Array(24));
  pushU16(output, 3);
  return Uint8Array.from(output);
}

function syntheticHmtx() {
  const output = [];
  for (const advance of [600, 600, 300]) {
    pushU16(output, advance);
    pushI16(output, 0);
  }
  return Uint8Array.from(output);
}

function syntheticLoca() {
  const glyphBytes = syntheticRectangleGlyph().byteLength;
  const output = [];
  for (const offset of [0, glyphBytes, glyphBytes * 2, glyphBytes * 2]) {
    pushU32(output, offset);
  }
  return Uint8Array.from(output);
}

function syntheticMaxp() {
  const output = [];
  pushU32(output, 0x00010000);
  pushU16(output, 3);
  return Uint8Array.from(output);
}

function syntheticPost() {
  const output = [];
  pushU32(output, 0x00030000);
  pushU32(output, 0);
  pushI16(output, -100);
  pushI16(output, 50);
  output.push(...new Uint8Array(20));
  return Uint8Array.from(output);
}

async function sha256Hex(bytes) {
  if (!globalThis.crypto?.subtle) {
    throw new Error("Web Crypto SHA-256 is unavailable");
  }
  const digest = new Uint8Array(await globalThis.crypto.subtle.digest("SHA-256", bytes));
  return [...digest].map((value) => value.toString(16).padStart(2, "0")).join("");
}

function columnName(index) {
  let value = index + 1;
  let output = "";
  while (value > 0) {
    value -= 1;
    output = String.fromCharCode(65 + (value % 26)) + output;
    value = Math.floor(value / 26);
  }
  return output;
}

function concatBytes(...parts) {
  const length = parts.reduce((total, part) => total + part.byteLength, 0);
  const output = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.byteLength;
  }
  return output;
}

function littleU16(value) {
  return Uint8Array.of(value & 0xff, (value >>> 8) & 0xff);
}

function littleU32(value) {
  return Uint8Array.of(
    value & 0xff,
    (value >>> 8) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 24) & 0xff
  );
}

function bigU32(value) {
  return Uint8Array.of(
    (value >>> 24) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 8) & 0xff,
    value & 0xff
  );
}

function pushU16(output, value) {
  output.push((value >>> 8) & 0xff, value & 0xff);
}

function pushI16(output, value) {
  pushU16(output, value < 0 ? 0x10000 + value : value);
}

function pushU32(output, value) {
  output.push(
    (value >>> 24) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 8) & 0xff,
    value & 0xff
  );
}

function crc32(bytes) {
  let value = 0xffffffff;
  for (const byte of bytes) {
    value ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      value = (value >>> 1) ^ (value & 1 ? 0xedb88320 : 0);
    }
  }
  return (value ^ 0xffffffff) >>> 0;
}

function adler32(bytes) {
  let first = 1;
  let second = 0;
  for (const byte of bytes) {
    first = (first + byte) % 65521;
    second = (second + first) % 65521;
  }
  return ((second << 16) | first) >>> 0;
}
