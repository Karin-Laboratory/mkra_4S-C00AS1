"use strict";
const http = require("http");
const fs = require("fs");
const path = require("path");

const BACKEND = process.env.MKRA_URL || "http://198.51.100.10:40772";
const HOST = process.env.LISTEN_HOST || "127.0.0.1";
const PORT = Number(process.env.LISTEN_PORT || 40773);
const CACHE = process.env.CACHE_FILE || "/var/lib/bs4k-epg/programs.json";

function readCacheFile(file) {
  try { const x = JSON.parse(fs.readFileSync(file, "utf8")); return Array.isArray(x.programs) ? x.programs : []; }
  catch (_) { return []; }
}
function readCache() {
  const by=new Map();
  for(const p of [...readCacheFile(CACHE),...readCacheFile(process.env.BS_CACHE_FILE||"/var/lib/bs4k-epg/bs-programs.json")]) by.set(p.id,p);
  return [...by.values()];
}
function queryPrograms(programs, query) {
  const n = (v) => v === null || v === undefined ? undefined : Number(v);
  const start = n(query.get("startAt")), end = n(query.get("endAt")), network = n(query.get("networkId")), service = n(query.get("serviceId"));
  return programs.filter(p => (start === undefined || Number(p.endAt) >= start) &&
    (end === undefined || Number(p.startAt) <= end) &&
    (network === undefined || Number(p.networkId) === network) &&
    (service === undefined || Number(p.serviceId) === service));
}
function mapLegacyPath(pathname) {
  return pathname.replace(/^\/api\/channels\/(?:BS|BS4K)\/([^/]+)/, (_,key)=>`/api/channels/${key.startsWith("BS_TS")?"BS":"BS4K"}/${key}`);
}
function translateJson(pathname, body) {
  if (!/^\/api\/(tuners|channels|services)(\/|$)/.test(pathname)) return body;
  return body.replace(/BS4K/g, "BS");
}
function guardBackend(p, res, idleMs, connectMs=6000) {
  const connectDeadline = setTimeout(() => p.destroy(new Error("backend connect timeout")), connectMs);
  p.on("socket", socket => {
    if (!socket.connecting) clearTimeout(connectDeadline);
    else socket.once("connect", () => clearTimeout(connectDeadline));
  });
  p.setTimeout(idleMs, () => p.destroy(new Error("backend idle timeout")));
  p.on("close", () => clearTimeout(connectDeadline));
  p.on("error", e => {
    if (!res.headersSent) res.writeHead(502, {"content-type":"text/plain; charset=utf-8"});
    if (!res.writableEnded) res.end(String(e));
  });
  // If the browser or recorder disconnects, close the backend request too so
  // mkra can release its subscriber/tuner instead of keeping an orphan stream.
  res.on("close", () => { if (!res.writableEnded) p.destroy(); });
}
function serviceItemId(program) {
  return `${Number(program.networkId)}${String(Number(program.serviceId)).padStart(5, "0")}`;
}
function streamFromService(req, res, program, u) {
  const b = new URL(BACKEND);
  const backendPath = `/api/services/${serviceItemId(program)}/stream${u.search}`;
  const p = http.request({hostname:b.hostname, port:b.port || 80, path:backendPath, method:req.method,
    headers:{...req.headers, host:b.host}}, r => {
      const headers = {...r.headers};
      res.writeHead(r.statusCode, headers);
      r.pipe(res);
    });
  // mkra's service stream has no programme boundary. Bound the cached
  // programme here so EPGStation receives EOF without a private client shim.
  // The cached end is a snapshot: extension/time-change tracking is not done.
  const endAt = Number(program.endAt || (Number(program.startAt) + Number(program.duration)));
  if (!Number.isFinite(endAt)) {
    p.destroy();
    res.writeHead(502); res.end("invalid programme end time"); return;
  }
  const remaining = endAt + 1000 - Date.now();
  if (remaining <= 0 || remaining > 0x7fffffff) {
    p.destroy();
    res.writeHead(410); res.end("programme is outside the stream window"); return;
  }
  const deadline = setTimeout(() => { p.destroy(); res.end(); }, remaining);
  res.once("close", () => clearTimeout(deadline));
  res.once("finish", () => clearTimeout(deadline));
  guardBackend(p, res, 15000);
  req.pipe(p);
}
function forwardStream(req, res, u) {
  const b = new URL(BACKEND);
  const p = http.request({hostname:b.hostname, port:b.port || 80, path:mapLegacyPath(u.pathname) + u.search, method:req.method,
    headers:{...req.headers, host:b.host}}, r => {
      res.writeHead(r.statusCode, r.headers);
      r.pipe(res);
    });
  guardBackend(p, res, 15000);
  req.pipe(p);
}
function proxy(req, res) {
  const u = new URL(req.url, "http://127.0.0.1");
  if (u.pathname === "/api/programs" && req.method === "GET") {
    const body = JSON.stringify(queryPrograms(readCache(), u.searchParams));
    res.writeHead(200, {"content-type":"application/json; charset=utf-8", "content-length":Buffer.byteLength(body), "x-bs4k-epg-cache":"1"});
    return res.end(body);
  }
  const servicePrograms=u.pathname.match(/^\/api\/services\/(\d+)\/programs$/);
  if(servicePrograms&&req.method==="GET") {
    const id=Number(servicePrograms[1]),body=JSON.stringify(readCache().filter(p=>Number(p.networkId)*100000+Number(p.serviceId)===id));
    res.writeHead(200,{"content-type":"application/json; charset=utf-8"});return res.end(body);
  }
  const programMatch = u.pathname.match(/^\/api\/programs\/(\d+)(\/stream)?$/);
  if (programMatch && req.method === "GET") {
    const id = Number(programMatch[1]);
    const program = readCache().find(p => Number(p.id) === id);
    if (!program) {
      const body = JSON.stringify({code:404, reason:"program not found"});
      res.writeHead(404, {"content-type":"application/json; charset=utf-8", "content-length":Buffer.byteLength(body)});
      return res.end(body);
    }
    console.log(`program ${programMatch[2] ? "stream" : "get"} id=${id} service=${program.serviceId} network=${program.networkId} backendService=${serviceItemId(program)}`);
    if (programMatch[2]) return streamFromService(req, res, program, u);
    const body = JSON.stringify(program);
    res.writeHead(200, {"content-type":"application/json; charset=utf-8", "content-length":Buffer.byteLength(body)});
    return res.end(body);
  }
  if (req.method === "GET" && /\/stream$/.test(u.pathname)) return forwardStream(req, res, u);
  const b = new URL(BACKEND);
  const mergedList=/^\/api\/channels\/(BS|BS4K)$/.test(u.pathname);
  const backendPath = (mergedList?"/api/channels":mapLegacyPath(u.pathname)) + u.search;
  const p = http.request({hostname:b.hostname, port:b.port || 80, path:backendPath, method:req.method,
    headers:{...req.headers, host:b.host}}, r => {
      const chunks = [];
      r.on("data", chunk => chunks.push(chunk));
      r.on("end", () => {
        const body = Buffer.concat(chunks);
        const type = String(r.headers["content-type"] || "");
        if (type.includes("application/json")) {
          let translated=translateJson(u.pathname, body.toString("utf8"));
          if(mergedList) translated=JSON.stringify(JSON.parse(translated).filter(c=>c.type==="BS"));
          const out = Buffer.from(translated);
          const headers = {...r.headers, "content-length": out.length};
          delete headers["transfer-encoding"];
          res.writeHead(r.statusCode, headers); res.end(out);
        } else {
          res.writeHead(r.statusCode, r.headers); res.end(body);
        }
    });
  });
  guardBackend(p, res, 10000);
  req.pipe(p);
}
http.createServer(proxy).listen(PORT, HOST, () => console.log(`bs4k-epg-proxy ${HOST}:${PORT} -> ${BACKEND}`));
