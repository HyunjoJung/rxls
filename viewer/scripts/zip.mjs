import { inflateRawSync } from "node:zlib";

const LOCAL_FILE_HEADER = 0x04034b50;
const CENTRAL_FILE_HEADER = 0x02014b50;
const END_OF_CENTRAL_DIRECTORY = 0x06054b50;
const UTF8_FLAG = 0x0800;
const DOS_DATE_1980_01_01 = 0x0021;

export function createStoredZip(entries) {
  if (!Array.isArray(entries) || entries.length > 0xffff) {
    throw new TypeError("entries must be a ZIP32-sized array");
  }
  const names = new Set();
  const localChunks = [];
  const centralChunks = [];
  let localOffset = 0;

  for (const entry of entries) {
    const name = String(entry?.name ?? "");
    if (!name || name.includes("\\") || name.startsWith("/") || names.has(name)) {
      throw new TypeError(`invalid or duplicate ZIP entry: ${name}`);
    }
    names.add(name);
    const nameBytes = Buffer.from(name, "utf8");
    const data = Buffer.isBuffer(entry.data)
      ? entry.data
      : Buffer.from(entry.data instanceof Uint8Array ? entry.data : String(entry.data ?? ""));
    const checksum = crc32(data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(LOCAL_FILE_HEADER, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(UTF8_FLAG, 6);
    local.writeUInt16LE(0, 8);
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(DOS_DATE_1980_01_01, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(0, 28);
    localChunks.push(local, nameBytes, data);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(CENTRAL_FILE_HEADER, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(UTF8_FLAG, 8);
    central.writeUInt16LE(0, 10);
    central.writeUInt16LE(0, 12);
    central.writeUInt16LE(DOS_DATE_1980_01_01, 14);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt16LE(0, 30);
    central.writeUInt16LE(0, 32);
    central.writeUInt16LE(0, 34);
    central.writeUInt16LE(0, 36);
    central.writeUInt32LE(0, 38);
    central.writeUInt32LE(localOffset, 42);
    centralChunks.push(central, nameBytes);
    localOffset += local.length + nameBytes.length + data.length;
  }

  const centralDirectory = Buffer.concat(centralChunks);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(END_OF_CENTRAL_DIRECTORY, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralDirectory.length, 12);
  end.writeUInt32LE(localOffset, 16);
  end.writeUInt16LE(0, 20);
  return Buffer.concat([...localChunks, centralDirectory, end]);
}

export function readZipEntries(input) {
  const bytes = Buffer.isBuffer(input)
    ? input
    : Buffer.from(input.buffer, input.byteOffset, input.byteLength);
  const endOffset = findEndRecord(bytes);
  const entryCount = bytes.readUInt16LE(endOffset + 10);
  const centralSize = bytes.readUInt32LE(endOffset + 12);
  const centralOffset = bytes.readUInt32LE(endOffset + 16);
  if (centralOffset + centralSize > endOffset) {
    throw new Error("ZIP central directory is out of bounds");
  }

  const entries = new Map();
  let offset = centralOffset;
  for (let index = 0; index < entryCount; index += 1) {
    assertSignature(bytes, offset, CENTRAL_FILE_HEADER, "central file header");
    const flags = bytes.readUInt16LE(offset + 8);
    const method = bytes.readUInt16LE(offset + 10);
    const checksum = bytes.readUInt32LE(offset + 16);
    const compressedSize = bytes.readUInt32LE(offset + 20);
    const uncompressedSize = bytes.readUInt32LE(offset + 24);
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    const localOffset = bytes.readUInt32LE(offset + 42);
    const nameStart = offset + 46;
    const nameEnd = nameStart + nameLength;
    const name = bytes.subarray(nameStart, nameEnd).toString(flags & UTF8_FLAG ? "utf8" : "latin1");
    if ((flags & 1) !== 0 || entries.has(name)) {
      throw new Error(`unsupported or duplicate ZIP entry: ${name}`);
    }

    assertSignature(bytes, localOffset, LOCAL_FILE_HEADER, "local file header");
    const localNameLength = bytes.readUInt16LE(localOffset + 26);
    const localExtraLength = bytes.readUInt16LE(localOffset + 28);
    const dataStart = localOffset + 30 + localNameLength + localExtraLength;
    const dataEnd = dataStart + compressedSize;
    if (dataEnd > bytes.length) {
      throw new Error(`ZIP entry is out of bounds: ${name}`);
    }
    const compressed = bytes.subarray(dataStart, dataEnd);
    const data =
      method === 0
        ? Buffer.from(compressed)
        : method === 8
          ? inflateRawSync(compressed)
          : (() => {
              throw new Error(`unsupported ZIP compression method ${method}: ${name}`);
            })();
    if (data.length !== uncompressedSize || crc32(data) !== checksum) {
      throw new Error(`ZIP entry integrity check failed: ${name}`);
    }
    entries.set(name, data);
    offset = nameEnd + extraLength + commentLength;
  }
  return entries;
}

function findEndRecord(bytes) {
  const minimum = Math.max(0, bytes.length - 65_557);
  for (let offset = bytes.length - 22; offset >= minimum; offset -= 1) {
    if (bytes.readUInt32LE(offset) === END_OF_CENTRAL_DIRECTORY) {
      return offset;
    }
  }
  throw new Error("ZIP end record was not found");
}

function assertSignature(bytes, offset, expected, label) {
  if (offset < 0 || offset + 4 > bytes.length || bytes.readUInt32LE(offset) !== expected) {
    throw new Error(`invalid ZIP ${label}`);
  }
}

const CRC32_TABLE = Array.from({ length: 256 }, (_, value) => {
  let result = value;
  for (let bit = 0; bit < 8; bit += 1) {
    result = (result >>> 1) ^ (result & 1 ? 0xedb88320 : 0);
  }
  return result >>> 0;
});

function crc32(bytes) {
  let result = 0xffffffff;
  for (const byte of bytes) {
    result = CRC32_TABLE[(result ^ byte) & 0xff] ^ (result >>> 8);
  }
  return (result ^ 0xffffffff) >>> 0;
}
