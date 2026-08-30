export const PRESERVATION_FIXTURE = Object.freeze({
  sourceFile: "apache-poi-simple-macro.xlsm",
  outputFile: "macro-preservation.xlsm",
  bytes: 13_796,
  sha256: "f76c986f4ebc25c2cc57c088b2511a1269f4bd61d6223a2ab58db351da348ba6",
  repository: "apache/poi",
  revision: "aa268199243921dd0d9e1dc8d96cc06331280c94",
  upstreamPath: "test-data/spreadsheet/SimpleMacro.xlsm"
});

export const PRESERVED_PARTS = [
  "[Content_Types].xml",
  "_rels/.rels",
  "xl/_rels/workbook.xml.rels",
  "xl/styles.xml",
  "xl/theme/theme1.xml",
  "xl/vbaProject.bin"
];
