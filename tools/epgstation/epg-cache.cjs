"use strict";

const fs = require("fs");
const path = require("path");

const TARGET_NETWORK_ID = 11;
const TARGET_SERVICE_IDS = new Set([101, 141, 151, 161, 171, 181, 211, 221]);
const RETAIN_PAST_MS = 6 * 60 * 60 * 1000;

function timestamp(value) {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && /^\d+$/.test(value)) return Number(value);
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : 0;
}

function validProgram(program) {
  return program && typeof program === "object" && !Array.isArray(program) &&
    Number.isSafeInteger(Number(program.networkId)) && Number(program.networkId) > 0 &&
    Number.isSafeInteger(Number(program.serviceId)) && Number(program.serviceId) > 0 &&
    program.eventId !== null && program.eventId !== "" && Number.isSafeInteger(Number(program.eventId)) && Number(program.eventId) >= 0 &&
    timestamp(program.startAt) > 0 &&
    timestamp(program.endAt ?? Number(program.startAt) + Number(program.duration)) > timestamp(program.startAt);
}

function eventKey(program) {
  return `${Number(program.networkId)}:${Number(program.serviceId)}:${Number(program.eventId)}`;
}

function validateCache(value, file) {
  if (!value || typeof value !== "object" || Array.isArray(value) || !Array.isArray(value.programs)) {
    throw new Error(`invalid EPG cache structure; preserved without overwrite: ${file}`);
  }
  for (const [index, program] of value.programs.entries()) {
    if (!validProgram(program)) {
      throw new Error(`invalid cached programme at index ${index}; preserved without overwrite: ${file}`);
    }
  }
  return { ...value, serviceStatus: value.serviceStatus || {}, report: value.report || [] };
}

function readCache(file) {
  let text;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return { version: 2, programs: [], serviceStatus: {}, report: [] };
    throw error;
  }
  let value;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw new Error(`EPG cache is malformed; preserved without overwrite: ${file}: ${error.message}`);
  }
  return validateCache(value, file);
}

function mergePrograms(existing, acquired) {
  const byKey = new Map();
  for (const program of existing) byKey.set(eventKey(program), program);
  for (const program of acquired) if (validProgram(program)) byKey.set(eventKey(program), program);
  return [...byKey.values()];
}

function retainCurrent(programs, now = Date.now()) {
  const cutoff = now - RETAIN_PAST_MS;
  return programs.filter(program =>
    Number(program.networkId) === TARGET_NETWORK_ID &&
    TARGET_SERVICE_IDS.has(Number(program.serviceId)) &&
    timestamp(program.endAt ?? Number(program.startAt) + Number(program.duration)) >= cutoff
  );
}

function writeCacheAtomic(file, value) {
  const directory = path.dirname(file);
  fs.mkdirSync(directory, { recursive: true });
  const temporary = `${file}.tmp.${process.pid}.${Date.now()}`;
  let mode = 0o600;
  try {
    const existing = fs.lstatSync(file);
    if (existing.isSymbolicLink()) throw new Error(`refusing symlink EPG cache: ${file}`);
    mode = existing.mode & 0o777;
  } catch (error) { if (error.code !== "ENOENT") throw error; }
  let descriptor;
  try {
    descriptor = fs.openSync(temporary, "wx", mode);
    fs.writeFileSync(descriptor, JSON.stringify(value, null, 2));
    fs.fsyncSync(descriptor);
    fs.closeSync(descriptor);
    descriptor = undefined;
    fs.renameSync(temporary, file);
    const dirFd = fs.openSync(directory, "r");
    try { fs.fsyncSync(dirFd); } finally { fs.closeSync(dirFd); }
  } catch (error) {
    if (descriptor !== undefined) fs.closeSync(descriptor);
    try { fs.unlinkSync(temporary); } catch (_) {}
    throw error;
  }
}

module.exports = { TARGET_NETWORK_ID, TARGET_SERVICE_IDS, eventKey, validProgram, readCache, mergePrograms, retainCurrent, writeCacheAtomic };
