import { createStoredZip } from "../../../viewer/scripts/zip.mjs";

/** Synthetic, bounded workbook whose real sheet export name exceeds 120 chars. */
export function createExportFilenameFixture() {
  const fileName = `${"W".repeat(80)}.xlsx`;
  const sheetName = "S".repeat(31);
  const sml = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
  const odr = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
  const pr = "http://schemas.openxmlformats.org/package/2006/relationships";
  const bytes = createStoredZip([
    {
      name: "[Content_Types].xml",
      data: '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>'
    },
    { name: "_rels/.rels", data: `<Relationships xmlns="${pr}"><Relationship Id="rId1" Type="${odr}/officeDocument" Target="xl/workbook.xml"/></Relationships>` },
    { name: "xl/workbook.xml", data: `<workbook xmlns="${sml}" xmlns:r="${odr}"><sheets><sheet name="${sheetName}" sheetId="1" r:id="rId1"/></sheets></workbook>` },
    { name: "xl/_rels/workbook.xml.rels", data: `<Relationships xmlns="${pr}"><Relationship Id="rId1" Type="${odr}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="${odr}/styles" Target="styles.xml"/></Relationships>` },
    { name: "xl/styles.xml", data: `<styleSheet xmlns="${sml}"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>` },
    { name: "xl/worksheets/sheet1.xml", data: `<worksheet xmlns="${sml}"><dimension ref="A1:B2"/><sheetFormatPr defaultRowHeight="15"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Long filename export</t></is></c></row><row r="2"><c r="B2"><v>123</v></c></row></sheetData></worksheet>` }
  ]);
  return { fileName, sheetName, bytes };
}
