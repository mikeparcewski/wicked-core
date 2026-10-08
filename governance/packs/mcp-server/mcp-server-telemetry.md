---
id: mcp-server-telemetry
title: "MCP servers: OpenTelemetry traces and metrics"
status: active
date: 2026-10-06
enforcement_class: guidance
steering_type: operations
scope: wiki:governance
domain: mcp-server
confidence: 0.9
applies_to: [scope, design, build, test, security-review, observability-review, install]
---

# MCP servers: OpenTelemetry traces and metrics

A server that cannot be observed cannot be operated: when a tool call is slow or fails, the operator needs the span that says which tool, which request and which upstream call. OpenTelemetry (`@opentelemetry/sdk-node` 0.223.x, `@opentelemetry/api` 1.9.x, Apache-2.0) is the vendor-neutral default; fixed span and instrument names let one dashboard read every server. Exporters stay off unless the operator sets an OTLP endpoint, so the default emits nothing off the host.

## Rules

- `MCPS-1002` (error): Traces and metrics are OpenTelemetry through @opentelemetry/sdk-node started before the server: one span per tool call (mcp.tool.call with mcp.server.name, mcp.tool.name, mcp.request.id and the outcome) and one child span per upstream request, the instruments mcp.tool.calls, mcp.tool.duration and mcp.tool.errors, and OTLP exporters armed only by the standard OTEL_EXPORTER_OTLP_* environment variables — with none set, nothing leaves the process.

## Sources

- OpenTelemetry JS SDK — https://github.com/open-telemetry/opentelemetry-js
- OpenTelemetry environment variable specification (OTEL_EXPORTER_OTLP_*) — https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/
