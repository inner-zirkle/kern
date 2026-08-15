# Security

The canonical security and trust-model documentation lives on the docs site:

- **Published:** https://yesitsfebreeze.github.io/kern/concepts/security
- **Source:** [`docs/site/content/docs/concepts/security.mdx`](site/content/docs/concepts/security.mdx)

It covers the full posture — local attack surface (Unix socket, MCP transport),
data at rest, what leaves the machine (LLM endpoints, gossip seed), and the
complete federation trust model with per-mechanism reasoning and current
`src/…:line` citations. This file is a pointer, not a second copy: previous
versions of this document drifted from the code, and the site page is the one
kept verified.

## TL;DR

- A default kern opens no network port. Gossip is off by default.
- When enabled, federation is unauthenticated and unencrypted — run it only on
  a network segment where you trust every host (LAN, private subnet, WireGuard
  mesh).
- A malicious peer's blast radius is bounded but non-zero; the exact CAN/CANNOT
  tables are on the site page.

## Reporting

Report security issues privately before public disclosure, via
[GitHub security advisories](https://github.com/inner-zirkle/kern/security/advisories),
not a public issue.
