"use strict";
// One gz helper call: reads {tool, input} on stdin, sends it to gz with the
// bridge token, and prints gz's one-line JSON reply. Used by gz.js.

const net = require("net");

const address = process.env.GZ_BRIDGE || "";
const split = address.lastIndexOf(":");
const host = address.slice(0, split);
const port = Number(address.slice(split + 1));

function reply(text) {
  process.stdout.write(text);
}

let body = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => (body += chunk));
process.stdin.on("end", () => {
  const request = JSON.parse(body);
  request.token = process.env.GZ_BRIDGE_TOKEN;
  const socket = net.createConnection({ host, port }, () => {
    socket.write(JSON.stringify(request) + "\n");
  });
  let answer = "";
  socket.setEncoding("utf8");
  socket.on("data", (chunk) => {
    answer += chunk;
    const end = answer.indexOf("\n");
    if (end >= 0) {
      reply(answer.slice(0, end));
      socket.end();
    }
  });
  socket.on("error", (err) => {
    reply(JSON.stringify({ ok: false, error: `cannot reach gz: ${err.message}` }));
  });
});
