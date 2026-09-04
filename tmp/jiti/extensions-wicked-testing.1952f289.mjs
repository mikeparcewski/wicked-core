"use strict";Object.defineProperty(exports, "__esModule", { value: true });exports.default = _default;








var _nodeChild_process = await jitiImport("node:child_process");
var _nodeUrl = await jitiImport("node:url");
var _nodePath = await jitiImport("node:path"); // wicked-testing extension for pi (earendil-works/pi)
// Loaded via jiti — no compilation needed.
// Hooks into two lifecycle events:
//   session_start  → shows QE project status at session open
//   agent_end      → claim-nudge + surfaces recent reviewer verdict
//
// Installed at: ~/.pi/agent/extensions/wicked-testing.ts
// Hook scripts: ~/.pi/agent/extensions/wicked-testing-hooks/
const hooksDir = (0, _nodePath.join)((0, _nodePath.dirname)((0, _nodeUrl.fileURLToPath)("file:///Users/michael.parcewski/.pi/agent/extensions/wicked-testing.ts")), "wicked-testing-hooks");function runHook(hook, cwd) {try {(0, _nodeChild_process.spawnSync)("node", [(0, _nodePath.join)(hooksDir, hook)], { input: JSON.stringify({ cwd }), stdio: ["pipe", "ignore", "inherit"],
        timeout: 5000
      });
  } catch {/* graceful degradation — never fail the session */}
}

// Extension factory — pi passes ExtensionAPI; use pi.on() to register handlers.
function _default(pi) {
  // session_start fires when a session is started, loaded, or reloaded.
  pi.on("session_start", async (_event, ctx) => {
    const cwd = ctx.sessionManager?.session?.cwd ?? process.cwd();
    runHook("session-start.mjs", cwd);
  });

  // agent_end fires once per agent response (Stop equivalent).
  pi.on("agent_end", async (_event, ctx) => {
    const cwd = ctx.sessionManager?.session?.cwd ?? process.cwd();
    runHook("claim-nudge.mjs", cwd);
    runHook("subagent-verdict.mjs", cwd);
  });
} /* v9-b97ea81962ad6bb1 */
