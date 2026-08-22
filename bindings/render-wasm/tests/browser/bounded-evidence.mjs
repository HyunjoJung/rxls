export class BoundedEntryLog {
  #entries = [];
  #bytes = 0;

  constructor({ maxEntries, maxBytes, maxEntryBytes }) {
    for (const [label, value] of Object.entries({ maxEntries, maxBytes, maxEntryBytes })) {
      if (!Number.isSafeInteger(value) || value <= 0) {
        throw new TypeError(`${label} must be a positive safe integer`);
      }
    }
    if (maxEntryBytes > maxBytes) {
      throw new TypeError("maxEntryBytes cannot exceed maxBytes");
    }
    this.maxEntries = maxEntries;
    this.maxBytes = maxBytes;
    this.maxEntryBytes = maxEntryBytes;
  }

  add(value) {
    if (typeof value !== "string") {
      throw new TypeError("bounded log entries must be strings");
    }
    const bytes = Buffer.byteLength(value);
    if (
      bytes === 0 ||
      bytes > this.maxEntryBytes ||
      this.#entries.length >= this.maxEntries ||
      this.#bytes + bytes > this.maxBytes
    ) {
      throw new Error("bounded route/request log limit exceeded");
    }
    this.#entries.push(value);
    this.#bytes += bytes;
  }

  values() {
    return [...this.#entries];
  }
}

export class BoundedTextTail {
  #bytes = Buffer.alloc(0);
  #discardedBytes = 0;

  constructor(maxBytes) {
    if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0) {
      throw new TypeError("maxBytes must be a positive safe integer");
    }
    this.maxBytes = maxBytes;
  }

  append(value) {
    const incoming = Buffer.from(String(value));
    if (incoming.byteLength >= this.maxBytes) {
      this.#discardedBytes +=
        this.#bytes.byteLength + incoming.byteLength - this.maxBytes;
      this.#bytes = Buffer.from(
        incoming.subarray(incoming.byteLength - this.maxBytes)
      );
      return;
    }
    const retainedPrefixBytes = this.maxBytes - incoming.byteLength;
    if (this.#bytes.byteLength > retainedPrefixBytes) {
      this.#discardedBytes += this.#bytes.byteLength - retainedPrefixBytes;
      this.#bytes = this.#bytes.subarray(
        this.#bytes.byteLength - retainedPrefixBytes
      );
    }
    this.#bytes = Buffer.concat(
      [this.#bytes, incoming],
      this.#bytes.byteLength + incoming.byteLength
    );
  }

  text() {
    return this.#bytes.toString("utf8");
  }

  get discardedBytes() {
    return this.#discardedBytes;
  }
}
