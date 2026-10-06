# Changelog

## Unreleased

- Updated the bundled renderer to 0.3.0 and the build compiler to TypeScript 7.
- Validate the selected renderer against its exact manifest and lockfile pins
  during builds and VSIX verification.

## 0.1.0

- Added local read-only previews for the rxls spreadsheet compatibility set.
- Added sheet/page navigation, zoom, reload-on-change, and SVG/PNG export.
- Added bounded host/webview messaging, strict CSP, cross-platform E2E, and
  reproducible VSIX verification.
