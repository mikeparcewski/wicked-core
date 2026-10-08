---
id: mcp-server-input-and-egress
title: "MCP servers: input validation and egress pinning"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: security
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: input validation and egress pinning

A tool's arguments come from a model, so they are untrusted input. Validating them before the handler (zod 4.x for hand-written tools, the converted JSON Schema otherwise) and pinning every upstream request to the configured base URL keeps a crafted argument from turning the server into a proxy to another host or a header injector. A rate limit that answers a user error keeps a looping agent from exhausting the upstream quota.

## Rules

- `MCPS-1005` (error): Every tool validates its input before the handler runs — a zod schema for hand-written tools, the OpenAPI-derived JSON Schema for converted ones — and every upstream request is pinned to the configured base URL (scheme, host, port, base path), sends only allowlisted arguments, never sets authentication or hop-by-hop headers from an argument, follows no redirect, and a per-tool rate limit answers a user error when exhausted.

## Sources

- fastmcp README (parameters, UserError) — https://github.com/punkpeye/fastmcp
- zod — https://github.com/colinhacks/zod
- OWASP Server-Side Request Forgery Prevention Cheat Sheet — https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html
