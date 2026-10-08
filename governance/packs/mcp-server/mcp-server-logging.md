---
id: mcp-server-logging
title: "MCP servers: structured logging"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: operations
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: structured logging

On the stdio transport stdout is the protocol channel, so a stray log line there corrupts the session; logs belong on stderr. `loglayer` (9.4.x, MIT) with `@loglayer/plugin-opentelemetry`, `@loglayer/transport-opentelemetry` and `@loglayer/plugin-redaction` gives one logger that correlates with the trace, exports through the same OTLP pipeline and redacts credential fields before a record is written.

## Rules

- `MCPS-1003` (error): Logs go through loglayer with the OpenTelemetry plugin stamping trace_id and span_id, a request-scoped child logger per tool call carried in AsyncLocalStorage, the OpenTelemetry transport for log records and a console transport on stderr — never stdout, which carries protocol frames — with the redaction plugin over credential fields and fastmcp's own logger routed into the same root.

## Sources

- loglayer docs — https://loglayer.dev
- loglayer OpenTelemetry plugin — https://loglayer.dev/plugins/opentelemetry.html
- loglayer OpenTelemetry transport — https://loglayer.dev/transports/opentelemetry.html
- fastmcp README (logger option) — https://github.com/punkpeye/fastmcp
