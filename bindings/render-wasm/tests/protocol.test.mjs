import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_FONT_BYTES,
  PROTOCOL,
  RenderProtocolError,
  encodeFontBundle,
  normalizeError,
  optionsJson,
  parseWorkerMessage,
  validateSvgOutput
} from "../js/protocol.mjs";

test("protocol accepts exact requests and rejects path-like identifiers", () => {
  assert.deepEqual(
    parseWorkerMessage({
      protocol: PROTOCOL,
      type: "request",
      requestId: "request-1",
      operation: "render-page",
      payload: { pageIndex: 0 }
    }),
    {
      protocol: PROTOCOL,
      type: "request",
      requestId: "request-1",
      operation: "render-page",
      payload: { pageIndex: 0 }
    }
  );
  assert.throws(
    () =>
      parseWorkerMessage({
        protocol: PROTOCOL,
        type: "request",
        requestId: "/private/request",
        operation: "render-page",
        payload: {}
      }),
    (error) => error instanceof RenderProtocolError && error.code === "invalid_request_id"
  );
});

test("font bundle encoding is deterministic, bounded, and path-safe", () => {
  const pack = {
    manifest: new TextEncoder().encode('{"schema":"rxls.render-font-pack.v1"}'),
    members: [
      { name: "fonts/a.ttf", bytes: Uint8Array.of(1, 2, 3) },
      { name: "LICENSE.txt", bytes: Uint8Array.of(4, 5) }
    ]
  };
  const first = encodeFontBundle(pack);
  const second = encodeFontBundle(pack);
  assert.deepEqual(first, second);
  assert.equal(new TextDecoder().decode(first.subarray(0, 8)), "RXLSFPK1");
  assert.throws(
    () =>
      encodeFontBundle({
        manifest: pack.manifest,
        members: [{ name: "../host-font.ttf", bytes: Uint8Array.of(1) }]
      }),
    (error) => error.code === "unsafe_font_path"
  );
  assert.throws(
    () =>
      encodeFontBundle({
        manifest: new Uint8Array(),
        members: [
          { name: "one.bin", bytes: new Uint8Array(MAX_FONT_BYTES / 2 + 1) },
          { name: "two.bin", bytes: new Uint8Array(MAX_FONT_BYTES / 2 + 1) }
        ]
      }),
    (error) => error.code === "limit_exceeded" && error.resource === "fontMemberBytes"
  );
});

test("options are JSON-only and bounded by structure", () => {
  assert.equal(optionsJson({ gridlines: false }), '{"gridlines":false}');
  assert.throws(() => optionsJson({ callback() {} }), /plain prototype|object/);
  let nested = {};
  let cursor = nested;
  for (let depth = 0; depth < 18; depth += 1) {
    cursor.next = {};
    cursor = cursor.next;
  }
  assert.throws(
    () => optionsJson(nested),
    (error) => error.code === "options_too_deep"
  );
  assert.throws(
    () => optionsJson({ values: new Array(4_097).fill(0) }),
    (error) => error.code === "limit_exceeded" && error.resource === "optionArrayItems"
  );
});

test("normalized errors remove host paths and keep typed limits", () => {
  const privatePath = ["", "Users", "alice", "private", "book.xlsx"].join("/");
  const normalized = normalizeError({
    code: "limit_exceeded",
    location: "input",
    resource: "inputBytes",
    limit: 8,
    actual: 9,
    message: `failed at ${privatePath}\nnext`
  });
  assert.equal(normalized.message, "failed at [path] next");
  assert.equal(normalized.resource, "inputBytes");
  assert.equal(normalized.limit, 8);
  assert.equal(normalized.actual, 9);
  assert.doesNotMatch(JSON.stringify(normalized), /Users|book\.xlsx/);
});

test("SVG validation allows embedded pixels and passive links only", () => {
  const safe = `<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg">
    <defs><clipPath id="clip-0"><rect width="1" height="1"/></clipPath></defs>
    <a href="https://example.com/?a=1&amp;b=2"><text clip-path="url(#clip-0)">ok</text></a>
    <image href="data:image/png;base64,iVBORw0KGgo="/>
  </svg>`;
  assert.equal(validateSvgOutput(safe), new TextEncoder().encode(safe).byteLength);
  assert.doesNotThrow(() =>
    validateSvgOutput('<svg><text>visible url(https://example.com/not-a-resource)</text></svg>')
  );

  for (const svg of [
    '<svg><script>alert(1)</script></svg>',
    '<svg><image href="https://example.com/pixel.png"/></svg>',
    '<svg><a href="javascript:alert(1)"><text>x</text></a></svg>',
    '<svg><a href="java&#x73;cript:alert(1)"><text>x</text></a></svg>',
    '<svg><a href="javascript&colon;alert(1)"><text>x</text></a></svg>',
    '<svg><rect fill="url(https://example.com/pattern.svg)"/></svg>',
    '<svg><image href=data:image/png;base64,iVBORw0KGgo=/></svg>',
    '<svg><rect onclick="alert(1)"/></svg>'
  ]) {
    assert.throws(
      () => validateSvgOutput(svg),
      (error) => error.code === "unsafe_svg" || error.code === "external_svg_resource"
    );
  }
});
