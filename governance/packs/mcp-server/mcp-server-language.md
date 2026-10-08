---
id: mcp-server-language
title: "MCP servers: language and framework"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: architecture
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: language and framework

One language and one framework keep every server this platform builds on the same skeleton, so the contract tests, the telemetry wiring and the reviews carry over from one server to the next. The default is the TypeScript `fastmcp` package (4.22.x, MIT), which gives both transports, per-tool `canAccess`, an `authenticate` callback and zod-validated parameters; the Python FastMCP is a different project and is not this default. Tool annotations matter because the broker derives a call's class from them (MCP-D4): a missing or flattering annotation changes which posture rule a call meets.

## Rules

- `MCPS-1001` (error): An MCP server this platform builds is TypeScript on Node 22 or later using the fastmcp package (punkpeye/fastmcp; not the Python FastMCP), one src/server.ts entry, the stdio transport by default and httpStream behind a flag, and every tool declares its annotations honestly so the broker's class derivation (MCP-D4) reads it as intended.

## Sources

- fastmcp README — https://github.com/punkpeye/fastmcp
- Model Context Protocol specification, tool annotations — https://modelcontextprotocol.io/specification
- The mcp-defaults pack, MCP-D4 (`governance/packs/mcp-defaults/mcp-defaults.md`)
