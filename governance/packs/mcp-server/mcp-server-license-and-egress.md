---
id: mcp-server-license-and-egress
title: "MCP servers: license headers and default egress"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: compliance
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: license headers and default egress

A generated server is code that someone ships, so its license must be stated in every file and at the root; SPDX identifiers make that machine-checkable. Data leaving the host is a compliance question before it is a technical one, so the default sends nothing and wires no vendor sink. When the upstream API's document declares terms, a NOTICE carries them with the server.

## Rules

- `MCPS-1007` (warn): Every source file starts with an SPDX license header and the server ships a LICENSE; no telemetry or log leaves the process unless the operator sets an OTLP endpoint, no vendor SDK or third-party sink is wired by default, and a NOTICE names the upstream API's terms when its document declares a license.

## Sources

- SPDX license identifiers — https://spdx.dev/learn/handling-license-info/
- OpenTelemetry JS SDK (exporters off unless configured) — https://github.com/open-telemetry/opentelemetry-js
- The mcp-defaults pack (`governance/packs/mcp-defaults/mcp-defaults.md`)
