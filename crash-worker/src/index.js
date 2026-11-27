const MAX_BODY_BYTES = 16 * 1024;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return new Response("ok");
    }
    if (url.pathname !== "/v1/crashes") {
      return new Response("Not found", { status: 404 });
    }
    if (request.method === "POST") {
      const reader = request.body?.getReader();
      if (!reader) return new Response("Expected JSON object", { status: 400 });
      const chunks = [];
      let size = 0;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > MAX_BODY_BYTES) {
          await reader.cancel();
          return new Response("Report too large", { status: 413 });
        }
        chunks.push(value);
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.byteLength;
      }
      let report;
      try {
        report = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
      } catch {
        return new Response("Invalid JSON", { status: 400 });
      }
      if (report === null || typeof report !== "object" || Array.isArray(report)) {
        return new Response("Expected JSON object", { status: 400 });
      }
      const key = `crash:${String(9999999999999 - Date.now()).padStart(13, "0")}:${crypto.randomUUID().slice(0, 8)}`;
      await env.CRASHES.put(key, JSON.stringify(report), { expirationTtl: 2592000 });
      return Response.json({ ok: true, id: key }, { status: 201 });
    }
    if (request.method === "GET") {
      if (!env.READ_TOKEN || request.headers.get("Authorization") !== `Bearer ${env.READ_TOKEN}`) {
        return new Response("Unauthorized", { status: 401 });
      }
      const requested = Number(url.searchParams.get("limit") ?? 20);
      const limit = Number.isFinite(requested) ? Math.max(1, Math.min(50, Math.trunc(requested))) : 20;
      const { keys } = await env.CRASHES.list({ prefix: "crash:", limit });
      const reports = await Promise.all(keys.map(({ name }) => env.CRASHES.get(name, "json")));
      return Response.json(reports.filter((report) => report !== null));
    }
    return new Response("Method not allowed", { status: 405, headers: { Allow: "GET, POST" } });
  },
};
