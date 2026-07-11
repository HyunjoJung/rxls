# Security Policy

`rxls` parses **untrusted binary files** (legacy `.xls`). Memory safety and
graceful handling of malicious or malformed input are primary goals, so security
reports are taken seriously.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x (latest) | ✅ |
| < 0.1.0 | ❌ |

Security fixes are applied to the latest release only.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub Issues.**

Use GitHub's private reporting (preferred):
[Report a vulnerability](https://github.com/HyunjoJung/rxls/security/advisories/new)

Or email the maintainer via the address on the [GitHub profile](https://github.com/HyunjoJung).

## What to Include

- Description of the vulnerability and potential impact
- A minimal proof-of-concept `.xls` (or bytes) that triggers it
- Affected version(s)
- Suggested fix, if available

## Response Timeline

| Stage | Target |
|-------|--------|
| Acknowledgement | Within 3 business days |
| Initial assessment | Within 7 business days |
| Fix & release | Critical: ASAP · High: within 30 days |

## Scope

In scope:
- Panics, out-of-bounds reads, unbounded allocation, or infinite loops on
  crafted `.xls` input (the crate is `#![forbid(unsafe_code)]`; any crash is a bug)
- Silent emission of incorrect/garbled text that could poison a downstream index

Out of scope:
- Third-party dependencies (report upstream; we update via Dependabot)
- Issues requiring local machine access or social engineering

## Disclosure Policy

Coordinated disclosure: fixes ship before public disclosure. Reporters are
credited in the release notes unless anonymity is requested. There is no bug
bounty program.
