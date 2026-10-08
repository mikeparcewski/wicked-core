---
id: mcp-server-authentication
title: "MCP servers: authentication and the one secret"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: security
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: authentication and the one secret

An MCP server holds a credential to someone else's system, so its authentication is the control that matters most. wicked-crew's broker and registry inject exactly one environment variable into a server they launch, so the server reads one secret and keeps every non-secret parameter (header name, token URL, client id) in a committed config file. Failing at startup on a missing secret turns a silent misconfiguration into a named error; fastmcp's `authenticate` and per-tool `canAccess` close the hosted transport.

## Rules

- `MCPS-1004` (critical): Every tool and resource is authenticated per the upstream's scheme (bearer, API key, basic or OAuth2 client credentials); the server reads ONE secret from the environment variable <SERVER>_TOKEN, named in .env.example, and every non-secret authentication parameter from its committed mcp-server.config.json — because the broker and the registry inject exactly one variable; a secret never appears in code, tool arguments, logs or results; a missing secret fails at startup naming the variable; the httpStream transport requires the authenticate callback and every tool carries canAccess; a stdio server inherits the broker's environment injection (MCP-D2).

## Sources

- fastmcp README (authenticate, canAccess) — https://github.com/punkpeye/fastmcp
- The mcp-defaults pack, MCP-D2 (`governance/packs/mcp-defaults/mcp-defaults.md`)
