---
id: mcp-server-contract-tests
title: "MCP servers: contract tests and the conformance smoke"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: testing
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: contract tests and the conformance smoke

The engine's verify floor runs a repository's own typecheck, lint and test scripts, so a server whose checks live under those scripts is judged on evidence rather than on a worker's claim. A contract test per tool pins the request mapping; the conformance smoke proves the protocol handshake and, more importantly, the two failure paths an operator meets first: a missing credential and an unauthenticated hosted call.

## Rules

- `MCPS-1006` (error): Every tool has a contract test (from the OpenAPI examples for converted tools, hand-written otherwise) and the server has a stdio conformance smoke covering initialize, tools/list, tools/call, the missing-credential startup failure and the httpStream 401 path, all under npm test, with typecheck, lint and test scripts present so the verify floor runs them.

## Sources

- Model Context Protocol specification (initialize, tools/list, tools/call) — https://modelcontextprotocol.io/specification
- fastmcp README (testing with the MCP inspector and client) — https://github.com/punkpeye/fastmcp
