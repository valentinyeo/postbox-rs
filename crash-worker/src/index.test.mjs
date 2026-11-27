import assert from "node:assert/strict";
import { test } from "node:test";
import worker from "./index.js";

function environment() {
  const reports = new Map();
  let listOptions;
  let ttl;
  return {
    READ_TOKEN: "test-read-token",
    reports,
    get listOptions() { return listOptions; },
    get ttl() { return ttl; },
    CRASHES: {
      async put(key, value, options) { reports.set(key, value); ttl = options.expirationTtl; },
      async list(options) {
        listOptions = options;
        return { keys: [...reports.keys()].filter((key) => key.startsWith(options.prefix)).sort().slice(0, options.limit).map((name) => ({ name })) };
      },
      async get(key) { return reports.has(key) ? JSON.parse(reports.get(key)) : null; },
    },
  };
}

function request(method, body, headers = {}, query = "") {
  return new Request(`https://crashes.test/v1/crashes${query}`, { method, body, headers });
}

test("health returns ok", async () => {
  const response = await worker.fetch(new Request("https://crashes.test/health"), environment());
  assert.equal(response.status, 200);
  assert.equal(await response.text(), "ok");
});

test("POST stores an object with a reverse-timestamp key and 30-day expiry", async () => {
  const env = environment();
  const payload = { kind: "panic", version: "0.1.0", backtrace: "frame\nframe" };
  const response = await worker.fetch(request("POST", JSON.stringify(payload)), env);
  assert.equal(response.status, 201);
  const result = await response.json();
  assert.equal(result.ok, true);
  assert.match(result.id, /^crash:\d{13}:[0-9a-f]{8}$/);
  assert.deepEqual(JSON.parse(env.reports.get(result.id)), payload);
  assert.equal(env.ttl, 2592000);
});

test("POST rejects invalid JSON and non-object JSON", async () => {
  for (const body of ["", "{", "[]", "null", "3", '"string"']) {
    const env = environment();
    const response = await worker.fetch(request("POST", body), env);
    assert.equal(response.status, 400, body);
    assert.equal(env.reports.size, 0);
  }
});

test("POST accepts exactly 16 KiB and rejects a larger streamed body", async () => {
  const boundary = JSON.stringify({ x: "a".repeat(16376) });
  assert.equal(new TextEncoder().encode(boundary).length, 16384);
  assert.equal((await worker.fetch(request("POST", boundary), environment())).status, 201);
  const env = environment();
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode(boundary));
      controller.enqueue(new Uint8Array([32]));
      controller.close();
    },
  });
  const streamed = new Request("https://crashes.test/v1/crashes", { method: "POST", body: stream, duplex: "half" });
  assert.equal((await worker.fetch(streamed, env)).status, 413);
  assert.equal(env.reports.size, 0);
});

test("GET refuses missing, wrong and unconfigured read tokens", async () => {
  for (const authorization of [undefined, "Bearer wrong", "Bearer test-read-token suffix"]) {
    const headers = authorization ? { Authorization: authorization } : {};
    assert.equal((await worker.fetch(request("GET", undefined, headers), environment())).status, 401);
  }
  const env = environment();
  delete env.READ_TOKEN;
  assert.equal((await worker.fetch(request("GET", undefined, { Authorization: "Bearer undefined" }), env)).status, 401);
});

test("GET reads newest reports with bounded limits and skips expired values", async () => {
  const env = environment();
  env.reports.set("crash:0000000000002:22222222", JSON.stringify({ kind: "older" }));
  env.reports.set("crash:0000000000001:11111111", JSON.stringify({ kind: "newest" }));
  env.reports.set("other", "{}");
  const headers = { Authorization: "Bearer test-read-token" };
  const response = await worker.fetch(request("GET", undefined, headers), env);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), [{ kind: "newest" }, { kind: "older" }]);
  assert.deepEqual(env.listOptions, { prefix: "crash:", limit: 20 });
  for (const [query, limit] of [["?limit=500", 50], ["?limit=1", 1], ["?limit=-1", 1], ["?limit=NaN", 20]]) {
    await worker.fetch(request("GET", undefined, headers, query), env);
    assert.equal(env.listOptions.limit, limit);
  }
  env.CRASHES.get = async () => null;
  assert.deepEqual(await (await worker.fetch(request("GET", undefined, headers), env)).json(), []);
});

test("unknown paths and unsupported methods do not store reports", async () => {
  const env = environment();
  assert.equal((await worker.fetch(new Request("https://crashes.test/nope"), env)).status, 404);
  assert.equal((await worker.fetch(request("PUT", "{}"), env)).status, 405);
  assert.equal(env.reports.size, 0);
});
