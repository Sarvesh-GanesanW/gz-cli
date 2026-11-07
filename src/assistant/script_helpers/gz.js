"use strict";
// File tools for gz run_script, served by gz's sandbox.
//
// Calls are synchronous so scripts read like plain code. Node has no
// synchronous sockets, so each call runs gz_call.js in a short-lived child
// `node` through execFileSync, which blocks until gz answers. A child per
// call costs a few tens of milliseconds, well within gz's 500-call limit,
// and needs no native modules or busy-waiting.

const { execFileSync } = require("child_process");
const path = require("path");

class ToolError extends Error {}

const CALLER = path.join(__dirname, "gz_call.js");
const MAX_REPLY_BYTES = 64 * 1024 * 1024;

function call(tool, fields) {
  const input = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined && value !== null) input[key] = value;
  }
  const reply = JSON.parse(
    execFileSync(process.execPath, [CALLER], {
      input: JSON.stringify({ tool, input }),
      maxBuffer: MAX_REPLY_BYTES,
    }).toString("utf8")
  );
  if (!reply.ok) throw new ToolError(reply.error || "the call failed");
  return reply.result;
}

module.exports = {
  ToolError,
  read_file: (p, offset, limit) => call("read_file", { path: p, offset, limit }),
  list_directory: (p = ".") => call("list_directory", { path: p }),
  find_files: (pattern, p) => call("find_files", { pattern, path: p }),
  search_files: (pattern, p, glob) => call("search_files", { pattern, path: p, glob }),
  edit_file: (p, old_text, new_text, replace_all = false) =>
    call("edit_file", { path: p, old_text, new_text, replace_all }),
  write_file: (p, content) => call("write_file", { path: p, content }),
};
