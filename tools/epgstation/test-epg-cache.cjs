"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { readCache, mergePrograms, retainCurrent, writeCacheAtomic } = require("./epg-cache.cjs");

const now = Date.now();
function event(networkId, serviceId, eventId, startAt, endAt, title = "") {
  return { id: networkId * 10000000000 + serviceId * 100000 + eventId, networkId, serviceId, eventId, startAt, endAt, title };
}

test("all target services are independently retained and reported by service key", () => {
  const ids = [101,141,151,161,171,181,211,221];
  const programs = ids.map((id, index) => event(11, id, index + 1, now, now + 3600000));
  assert.deepEqual(retainCurrent(programs, now).map(p => p.serviceId), ids);
});

test("partial acquisition merges new events without deleting other services", () => {
  const before = [event(11,101,1,now,now+3600000), event(11,141,2,now,now+3600000)];
  const after = mergePrograms(before, [event(11,101,3,now,now+7200000)]);
  assert.equal(after.length, 3);
  assert.ok(after.some(p => p.serviceId === 141 && p.eventId === 2));
});

test("SQLite-restored EPGStation row without lastSuccess remains while its endAt is in the future", () => {
  const cache = { programs:[event(11,141,10,now+3600000,now+7200000)], report:[] };
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "epg-legacy-"));
  try {
    const file = path.join(directory, "programs.json");
    fs.writeFileSync(file, JSON.stringify(cache));
    const restored = readCache(file);
    assert.equal(restored.programs.length, 1);
    assert.equal(retainCurrent(restored.programs, now).length, 1);
  } finally { fs.rmSync(directory, { recursive:true, force:true }); }
});

test("malformed acquired entries are ignored instead of entering the cache", () => {
  const merged = mergePrograms([], [event(11,141,1,now,now+1000), {networkId:11,serviceId:141,startAt:now,endAt:now+1000}]);
  assert.equal(merged.length, 1);
});

test("event update replaces the same network/service/event key and extension", () => {
  const previous = event(11,141,7,now,now+1800000,"old");
  const updated = event(11,141,7,now,now+5400000,"extended");
  const merged = mergePrograms([previous], [updated]);
  assert.equal(merged.length, 1);
  assert.equal(merged[0].title, "extended");
  assert.equal(merged[0].endAt, now+5400000);
});

test("networkId 4 and 11 never collide; 4K output excludes networkId 4", () => {
  const bs = event(4,141,7,now,now+3600000);
  const bs4k = event(11,141,7,now,now+3600000);
  const merged = mergePrograms([bs], [bs4k]);
  assert.equal(merged.length, 2);
  assert.deepEqual(retainCurrent(merged, now), [bs4k]);
});

test("expired programmes are pruned by endAt even if lastSuccess metadata is stale", () => {
  const rows = [event(11,141,1,now-8*86400000,now-7*86400000)];
  assert.deepEqual(retainCurrent(rows, now), []);
});

test("corrupt cache is rejected and left byte-for-byte intact", () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "epg-corrupt-"));
  try {
    const file = path.join(directory, "programs.json"), original = "{broken";
    fs.writeFileSync(file, original);
    assert.throws(() => readCache(file), /malformed; preserved/);
    assert.equal(fs.readFileSync(file, "utf8"), original);
  } finally { fs.rmSync(directory, { recursive:true, force:true }); }
});

test("atomic rename failure leaves the previous cache untouched", () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "epg-write-fail-"));
  const file = path.join(directory, "programs.json"), original = JSON.stringify({ programs:[event(11,101,1,now,now+1000)] });
  const rename = fs.renameSync;
  try {
    fs.writeFileSync(file, original);
    fs.renameSync = () => { const error = new Error("synthetic rename failure"); error.code = "EIO"; throw error; };
    assert.throws(() => writeCacheAtomic(file, { programs:[] }), /synthetic rename failure/);
    fs.renameSync = rename;
    assert.equal(fs.readFileSync(file, "utf8"), original);
    assert.deepEqual(fs.readdirSync(directory), ["programs.json"]);
    fs.renameSync = rename;
    writeCacheAtomic(file, { programs:[event(11,101,2,now,now+2000)] });
    assert.equal(readCache(file).programs[0].eventId, 2, "a later successful retry can replace the cache atomically");
  } finally { fs.renameSync = rename; fs.rmSync(directory, { recursive:true, force:true }); }
});
