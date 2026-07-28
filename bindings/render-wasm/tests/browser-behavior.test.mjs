import assert from "node:assert/strict";
import test from "node:test";

import { RenderWorkerClient } from "../js/client.mjs";
import {
  BROWSER_BEHAVIOR_PROOF_SCHEMA,
  validateBrowserBehaviorProof
} from "./browser/scenario.mjs";
import {
  provePendingResourceBoundary,
  validatePendingResourceBoundaryProof
} from "./browser/proof.mjs";

const sha256 = (character) => character.repeat(64);

function validProof() {
  const paper = { widthRaw: 102_400, heightRaw: 204_800 };
  return {
    schema: BROWSER_BEHAVIOR_PROOF_SCHEMA,
    fixture: {
      workbookBytes: 1_000,
      workbookSha256: sha256("1"),
      fontPackSha256: sha256("2"),
      renderedImageBytes: 100,
      renderedImageSha256: sha256("3")
    },
    capabilitiesSha256: sha256("4"),
    cancellation: {
      abortSignal: "AbortError",
      activeOpen: "AbortError",
      reopenedDocument: true
    },
    progress: [
      { completed: 0, total: 3, stage: "accepted" },
      { completed: 1, total: 3, stage: "parsing" },
      { completed: 2, total: 3, stage: "finalizing" },
      { completed: 3, total: 3, stage: "complete" }
    ],
    pendingBoundary: {
      inputBytes: 32 * 1024 * 1024,
      queuedRequests: 4,
      pendingResourceBytes: 128 * 1024 * 1024,
      overflowBytes: 1,
      overflowOutcome: {
        synchronous: true,
        code: "limit_exceeded",
        resource: "pendingResourceBytes"
      },
      rejectedRequests: 4,
      rejectionCode: "client_closed",
      dispatchedRequests: 0,
      transportTerminated: true
    },
    limits: {
      fontFiles: { code: "limit_exceeded", resource: "fontFiles" },
      hardPage: { code: "limit_exceeded", resource: "pages" },
      dpi: { code: "dpi_out_of_range", resource: null },
      outputBytes: { code: "limit_exceeded", resource: "output_bytes" },
      imageCount: { code: "limit_exceeded", resource: "maxImages" },
      imageBytes: { code: "limit_exceeded", resource: "maxImageBytes" }
    },
    tile: {
      firstRow: 0,
      firstCol: 0,
      lastRow: 63,
      lastCol: 31,
      bytes: 250_000,
      sha256: sha256("5")
    },
    pages: {
      count: 8,
      paper,
      first: {
        pageIndex: 0,
        responsePageIndex: 0,
        pageMapSha256: sha256("6"),
        svg: {
          bytes: 1_000,
          sha256: sha256("7"),
          ...paper
        }
      },
      nonzero: {
        pageIndex: 7,
        responsePageIndex: 7,
        pageMapSha256: sha256("8"),
        svg: {
          bytes: 1_000,
          sha256: sha256("9"),
          repeatSha256: sha256("9"),
          ...paper
        },
        png: {
          bytes: 1_000,
          sha256: sha256("a"),
          width: 100,
          height: 200,
          dpi: 96
        }
      },
      outOfRange: {
        pageIndex: 8,
        code: "page_index_out_of_range"
      }
    },
    hardStop: { deadlineMs: 2_000, rejectedRequests: 2 },
    network: {
      cspNegativeControl: true,
      unexpectedExternalResources: 0
    }
  };
}

test("browser behavior proof accepts the bounded parity contract", () => {
  const proof = validProof();
  assert.deepEqual(JSON.parse(validateBrowserBehaviorProof(proof)), proof);
});

test("browser behavior proof fails closed on page-isolation drift", () => {
  const proof = validProof();
  proof.pages.nonzero.svg.sha256 = proof.pages.first.svg.sha256;
  proof.pages.nonzero.svg.repeatSha256 = proof.pages.first.svg.sha256;
  assert.throws(
    () => validateBrowserBehaviorProof(proof),
    /hashes are not isolated/
  );
});

test("browser behavior proof binds PNG geometry and deterministic SVG output", () => {
  const invalidPng = validProof();
  invalidPng.pages.nonzero.png.width += 1;
  assert.throws(
    () => validateBrowserBehaviorProof(invalidPng),
    /PNG geometry/
  );

  const invalidRepeat = validProof();
  invalidRepeat.pages.nonzero.svg.repeatSha256 = sha256("b");
  assert.throws(
    () => validateBrowserBehaviorProof(invalidRepeat),
    /SVG repeat/
  );
});

test("browser behavior proof binds the exact preclone pending-resource boundary", () => {
  const invalidTotal = validProof();
  invalidTotal.pendingBoundary.pendingResourceBytes -= 1;
  assert.throws(
    () => validateBrowserBehaviorProof(invalidTotal),
    /pending-resource behavior proof/
  );

  const invalidOverflow = validProof();
  invalidOverflow.pendingBoundary.overflowOutcome.resource = "inputBytes";
  assert.throws(
    () => validateBrowserBehaviorProof(invalidOverflow),
    /pending-resource behavior proof/
  );

  const invalidTermination = validProof();
  invalidTermination.pendingBoundary.rejectedRequests = 3;
  assert.throws(
    () => validateBrowserBehaviorProof(invalidTermination),
    /pending-resource behavior proof/
  );
});

test("pending-resource control proof binds held and terminated lifecycle phases", () => {
  const held = {
    schema: "rxls.pending-resource-boundary.v1",
    phase: "held",
    nonce: "a".repeat(32),
    heldEpochMs: 1_000,
    releaseEpochMs: null,
    completedEpochMs: null,
    inputBytes: 32 * 1024 * 1024,
    queuedRequests: 4,
    pendingResourceBytes: 128 * 1024 * 1024,
    overflowBytes: 1,
    overflowOutcome: {
      synchronous: true,
      code: "limit_exceeded",
      resource: "pendingResourceBytes"
    },
    rejectedRequests: 0,
    rejectionCode: null,
    dispatchedRequests: 0,
    transportTerminated: false
  };
  assert.equal(validatePendingResourceBoundaryProof(held, "held"), held);
  const complete = {
    ...held,
    phase: "complete",
    releaseEpochMs: 1_100,
    completedEpochMs: 1_101,
    rejectedRequests: 4,
    rejectionCode: "client_closed",
    transportTerminated: true
  };
  assert.equal(
    validatePendingResourceBoundaryProof(complete, "complete"),
    complete
  );
  assert.throws(
    () =>
      validatePendingResourceBoundaryProof(
        { ...complete, rejectionCode: "worker_crashed" },
        "complete"
      ),
    /lifecycle proof/
  );
});

test("pending-resource proof materializes four distinct client clones", async () => {
  const pending = provePendingResourceBoundary({ RenderWorkerClient });
  for (let attempt = 0; attempt < 1_000; attempt += 1) {
    const control = globalThis.__rxlsPendingBoundaryProof;
    if (control?.phase === "held") {
      globalThis.__rxlsPendingBoundaryRelease = control.nonce;
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  assert.deepEqual(await pending, validProof().pendingBoundary);
});

test("pending-resource proof rejects clone-before-overflow-capacity clients", async () => {
  class CloneBeforeRejectClient extends RenderWorkerClient {
    open(bytes, options) {
      if (bytes.byteLength === 1) {
        bytes.slice();
      }
      return super.open(bytes, options);
    }
  }

  await assert.rejects(
    provePendingResourceBoundary({
      RenderWorkerClient: CloneBeforeRejectClient
    }),
    /overflow cloned bytes before capacity rejection/
  );
});
