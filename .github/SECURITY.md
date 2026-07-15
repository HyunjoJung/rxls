# Security Policy

`rxls` parses untrusted OLE2, ZIP, binary-record, and XML spreadsheet files.
Memory safety and graceful handling of malicious or malformed input are primary
goals, so security reports are taken seriously.

## Supported Versions

| Version | Supported |
|---------|-----------|
| `main` and the latest published release | Yes |
| All earlier releases | No |

Security fixes are applied to the latest release only.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub Issues.**

Use GitHub's private vulnerability reporting:
[Report a vulnerability](https://github.com/HyunjoJung/rxls/security/advisories/new)

## What to Include

- Description of the vulnerability and potential impact
- A minimal proof-of-concept spreadsheet (or bytes) that triggers it
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
- Panics, out-of-bounds reads, unbounded allocation, entity expansion, or
  infinite loops on crafted spreadsheet input
- Package-preserving edits that silently corrupt or drop untouched content
- Silent emission of incorrect/garbled text that could poison a downstream index

Out of scope:
- Third-party dependencies (report upstream; we update via Dependabot)
- Issues requiring local machine access or social engineering

## Disclosure Policy

Coordinated disclosure: fixes ship before public disclosure. Reporters are
credited in the release notes unless anonymity is requested. There is no bug
bounty program.
