/**
 * shunt relay — Cloudflare Worker
 *
 * Temporary encrypted dead-drop for credential transfer.
 *
 * POST /bundle           { code, payload }  → 201  (store; TTL 24h)
 * GET  /bundle/:code                        → 200  { payload }  (then delete)
 *
 * Security:
 *  - Payloads are AES-256-GCM encrypted client-side; this server sees only ciphertext.
 *  - Bundles are one-time: deleted immediately after the first successful GET.
 *  - 24-hour TTL as a backstop.
 *  - Rate-limited to 10 POST requests per IP per hour to prevent abuse.
 */

export interface Env {
  BUNDLES: KVNamespace;
}

const MAX_PAYLOAD_BYTES = 65_536; // 64 KB
const RATE_LIMIT_WINDOW = 3600;   // 1 hour in seconds
const RATE_LIMIT_MAX = 10;        // max pushes per IP per window

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function err(status: number, message: string): Response {
  return json({ error: message }, status);
}

function validateCode(code: string): boolean {
  return /^SH-[0-9a-f]{18}$/.test(code);
}

async function checkRateLimit(kv: KVNamespace, ip: string): Promise<boolean> {
  const key = `rl:${ip}`;
  const raw = await kv.get(key);
  const count = raw ? parseInt(raw, 10) : 0;
  if (count >= RATE_LIMIT_MAX) return false;
  await kv.put(key, String(count + 1), { expirationTtl: RATE_LIMIT_WINDOW });
  return true;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const { pathname, method } = { pathname: url.pathname, method: request.method };

    // ---------------------------------------------------------------------------
    // POST /bundle — upload encrypted bundle
    // ---------------------------------------------------------------------------
    if (method === "POST" && pathname === "/bundle") {
      const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
      if (!(await checkRateLimit(env.BUNDLES, ip))) {
        return err(429, "Too many requests. Try again later.");
      }

      let body: { code?: string; payload?: string };
      try {
        body = await request.json();
      } catch {
        return err(400, "Invalid JSON body.");
      }

      const { code, payload } = body;
      if (!code || !validateCode(code)) {
        return err(400, "Invalid or missing 'code'. Expected SH-<18 hex chars>.");
      }
      if (!payload || typeof payload !== "string") {
        return err(400, "Missing 'payload'.");
      }
      if (payload.length > MAX_PAYLOAD_BYTES) {
        return err(507, `Payload too large (max ${MAX_PAYLOAD_BYTES} bytes).`);
      }

      const kvKey = `bundle:${code}`;
      await env.BUNDLES.put(kvKey, JSON.stringify({ payload }), {
        expirationTtl: 86400, // 24 hours
      });

      const expiresAt = Math.floor(Date.now() / 1000) + 86400;
      return json({ ok: true, expires_at: expiresAt }, 201);
    }

    // ---------------------------------------------------------------------------
    // GET /bundle/:code — download and delete
    // ---------------------------------------------------------------------------
    const getMatch = pathname.match(/^\/bundle\/(SH-[0-9a-f]{18})$/);
    if (method === "GET" && getMatch) {
      const code = getMatch[1];
      const kvKey = `bundle:${code}`;

      const raw = await env.BUNDLES.get(kvKey);
      if (!raw) {
        return err(404, "Code not found or already used.");
      }

      // Delete immediately — one-time use
      await env.BUNDLES.delete(kvKey);

      let stored: { payload: string };
      try {
        stored = JSON.parse(raw);
      } catch {
        return err(500, "Corrupted bundle.");
      }

      return json({ payload: stored.payload });
    }

    return err(404, "Not found.");
  },
};
