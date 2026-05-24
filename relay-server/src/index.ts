/**
 * shunt relay-server
 *
 * Aggregates request events from multiple shunt instances into Postgres.
 * Serves a /status endpoint compatible with `shunt monitor` and a /history
 * endpoint for the history chart.
 *
 * POST /event         — shunt instance pushes a completed request event
 * POST /heartbeat     — shunt instance pushes its current status snapshot
 * GET  /status        — aggregated status (monitor-compatible)
 * GET  /history       — bucketed request counts for the history chart
 *                       ?window_ms=900000&bucket_ms=60000
 * GET  /health        — liveness check
 * GET  /stats         — event count + oldest event timestamp
 */

import {
  initSchema,
  insertEvent,
  upsertInstance,
  pruneOldEvents,
  getRecentEvents,
  getHistoryBuckets,
  getInstances,
  getStats,
} from "./db.js";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const PORT           = parseInt(process.env.PORT          ?? "3001");
const RELAY_TOKEN    = process.env.RELAY_TOKEN            ?? "";
const RETENTION_DAYS = parseInt(process.env.RETENTION_DAYS ?? "30");
const ONLINE_WINDOW_MS = 90_000; // instance is "online" if heartbeat seen in last 90s

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function err(status: number, message: string): Response {
  return json({ error: message }, status);
}

function authenticate(req: Request): boolean {
  if (!RELAY_TOKEN) return true;
  const auth = req.headers.get("authorization") ?? "";
  return auth === `Bearer ${RELAY_TOKEN}`;
}

function requireFields(body: Record<string, unknown>, fields: string[]): string | null {
  for (const f of fields) {
    if (body[f] === undefined || body[f] === null || body[f] === "") {
      return `Missing required field: ${f}`;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

await initSchema();

// Prune old events once on startup, then daily
await pruneOldEvents(RETENTION_DAYS);
setInterval(() => pruneOldEvents(RETENTION_DAYS), 24 * 60 * 60 * 1000);

console.log(`shunt relay-server listening on :${PORT}`);

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

Bun.serve({
  port: PORT,

  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    const { pathname } = url;
    const method = req.method;

    // -------------------------------------------------------------------------
    // POST /event — shunt instance pushes a completed request
    // -------------------------------------------------------------------------
    if (method === "POST" && pathname === "/event") {
      if (!authenticate(req)) return err(401, "Unauthorized");

      let body: Record<string, unknown>;
      try { body = await req.json(); }
      catch { return err(400, "Invalid JSON"); }

      const missing = requireFields(body, ["instance", "ts_ms", "account", "model"]);
      if (missing) return err(400, missing);

      await insertEvent({
        ts_ms:       Number(body.ts_ms),
        instance_id: String(body.instance),
        account:     String(body.account),
        model:       String(body.model),
        status_code: Number(body.status ?? 200),
        duration_ms: Number(body.duration_ms ?? 0),
      });

      return json({ ok: true }, 201);
    }

    // -------------------------------------------------------------------------
    // POST /heartbeat — instance pushes its current status snapshot
    // -------------------------------------------------------------------------
    if (method === "POST" && pathname === "/heartbeat") {
      if (!authenticate(req)) return err(401, "Unauthorized");

      let body: Record<string, unknown>;
      try { body = await req.json(); }
      catch { return err(400, "Invalid JSON"); }

      if (!body.instance) return err(400, "Missing required field: instance");

      await upsertInstance(
        String(body.instance),
        JSON.stringify(body.status ?? {}),
      );

      return json({ ok: true });
    }

    // -------------------------------------------------------------------------
    // GET /status — aggregated, monitor-compatible
    // -------------------------------------------------------------------------
    if (method === "GET" && pathname === "/status") {
      if (!authenticate(req)) return err(401, "Unauthorized");

      const [events, instances] = await Promise.all([
        getRecentEvents(200),
        getInstances(),
      ]);

      // Build accounts list by merging heartbeat snapshots.
      // If the same account name appears in multiple instances we take the one
      // from the most-recently-seen instance.
      const accountMap = new Map<string, Record<string, unknown>>();
      for (const inst of instances) {
        let snapshot: Record<string, unknown> = {};
        try { snapshot = JSON.parse(inst.status_json); } catch {}
        const seen = Number(inst.last_seen_ms);
        for (const acc of (snapshot.accounts as Record<string, unknown>[] | undefined) ?? []) {
          const name = String(acc.name ?? "");
          const prev = accountMap.get(name);
          if (!prev || seen > Number((prev as any)._seen ?? 0)) {
            accountMap.set(name, { ...acc, _seen: seen });
          }
        }
      }
      const accounts = Array.from(accountMap.values()).map(({ _seen: _, ...rest }) => rest);

      const recentRequests = events.map(e => ({
        ts_ms:        Number(e.ts_ms),
        account:      e.account,
        model:        e.model,
        status:       e.status_code,
        input_tokens: 0,
        output_tokens: 0,
        duration_ms:  Number(e.duration_ms),
      }));

      const now = Date.now();
      const onlineInstances = instances
        .filter(i => now - Number(i.last_seen_ms) < ONLINE_WINDOW_MS)
        .map(i => i.id);

      return json({
        accounts,
        recent_requests: recentRequests,
        pinned_account: null,
        last_used_account: recentRequests[0]?.account ?? null,
        instances: onlineInstances,
      });
    }

    // -------------------------------------------------------------------------
    // GET /history?window_ms=900000&bucket_ms=60000
    // -------------------------------------------------------------------------
    if (method === "GET" && pathname === "/history") {
      if (!authenticate(req)) return err(401, "Unauthorized");

      const windowMs = parseInt(url.searchParams.get("window_ms") ?? "900000");
      const bucketMs = parseInt(url.searchParams.get("bucket_ms") ?? "60000");

      if (isNaN(windowMs) || windowMs < 1000)
        return err(400, "window_ms must be >= 1000");
      if (isNaN(bucketMs) || bucketMs < 1000)
        return err(400, "bucket_ms must be >= 1000");
      if (windowMs > 8 * 24 * 60 * 60_000)
        return err(400, "window_ms too large (max 8d)");

      const nBuckets = Math.ceil(windowMs / bucketMs);
      const rows = await getHistoryBuckets(windowMs, bucketMs);

      return json({
        n_buckets: nBuckets,
        bucket_ms: bucketMs,
        window_ms: windowMs,
        buckets: rows.map(r => ({
          bucket:  Number(r.bucket),
          account: r.account,
          count:   Number(r.count),
        })),
      });
    }

    // -------------------------------------------------------------------------
    // GET /health
    // -------------------------------------------------------------------------
    if (method === "GET" && pathname === "/health") {
      return json({ ok: true });
    }

    // -------------------------------------------------------------------------
    // GET /stats
    // -------------------------------------------------------------------------
    if (method === "GET" && pathname === "/stats") {
      if (!authenticate(req)) return err(401, "Unauthorized");
      const stats = await getStats();
      return json(stats);
    }

    return err(404, "Not found");
  },
});
