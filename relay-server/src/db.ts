import postgres from "postgres";

const DATABASE_URL = process.env.DATABASE_URL;
if (!DATABASE_URL) throw new Error("DATABASE_URL is required");

export const sql = postgres(DATABASE_URL, {
  max: 10,
  idle_timeout: 30,
  connect_timeout: 10,
});

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

export async function initSchema() {
  await sql`
    CREATE TABLE IF NOT EXISTS instances (
      id            TEXT PRIMARY KEY,
      last_seen_ms  BIGINT NOT NULL,
      status_json   TEXT   NOT NULL DEFAULT '{}'
    )
  `;

  await sql`
    CREATE TABLE IF NOT EXISTS events (
      id           BIGSERIAL PRIMARY KEY,
      ts_ms        BIGINT   NOT NULL,
      instance_id  TEXT     NOT NULL,
      account      TEXT     NOT NULL,
      model        TEXT     NOT NULL,
      status_code  SMALLINT NOT NULL DEFAULT 200,
      duration_ms  INTEGER  NOT NULL DEFAULT 0
    )
  `;

  await sql`CREATE INDEX IF NOT EXISTS idx_events_ts      ON events (ts_ms DESC)`;
  await sql`CREATE INDEX IF NOT EXISTS idx_events_acc_ts  ON events (account, ts_ms DESC)`;
  await sql`CREATE INDEX IF NOT EXISTS idx_events_inst_ts ON events (instance_id, ts_ms DESC)`;

  console.log("Schema ready");
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

export async function insertEvent(e: {
  ts_ms: number;
  instance_id: string;
  account: string;
  model: string;
  status_code: number;
  duration_ms: number;
}) {
  await sql`
    INSERT INTO events (ts_ms, instance_id, account, model, status_code, duration_ms)
    VALUES (${e.ts_ms}, ${e.instance_id}, ${e.account}, ${e.model}, ${e.status_code}, ${e.duration_ms})
  `;
}

export async function upsertInstance(id: string, statusJson: string) {
  const now = Date.now();
  await sql`
    INSERT INTO instances (id, last_seen_ms, status_json)
    VALUES (${id}, ${now}, ${statusJson})
    ON CONFLICT (id) DO UPDATE
      SET last_seen_ms = ${now},
          status_json  = ${statusJson}
  `;
}

export async function pruneOldEvents(retentionDays: number) {
  const cutoff = Date.now() - retentionDays * 86_400_000;
  const result = await sql`DELETE FROM events WHERE ts_ms < ${cutoff}`;
  if (result.count > 0) {
    console.log(`Pruned ${result.count} events older than ${retentionDays}d`);
  }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

export async function getRecentEvents(limit = 200) {
  return sql<{
    ts_ms: string;
    instance_id: string;
    account: string;
    model: string;
    status_code: number;
    duration_ms: number;
  }[]>`
    SELECT ts_ms, instance_id, account, model, status_code, duration_ms
    FROM events
    ORDER BY ts_ms DESC
    LIMIT ${limit}
  `;
}

export async function getHistoryBuckets(windowMs: number, bucketMs: number) {
  const since = Date.now() - windowMs;
  return sql<{ bucket: number; account: string; count: number }[]>`
    SELECT
      FLOOR((ts_ms - ${since}::BIGINT) / ${bucketMs}::BIGINT)::INTEGER AS bucket,
      account,
      COUNT(*)::INTEGER AS count
    FROM events
    WHERE ts_ms >= ${since}
    GROUP BY bucket, account
    ORDER BY bucket ASC, account ASC
  `;
}

export async function getInstances() {
  return sql<{
    id: string;
    last_seen_ms: string;
    status_json: string;
  }[]>`
    SELECT id, last_seen_ms, status_json
    FROM instances
    ORDER BY last_seen_ms DESC
  `;
}

export async function getStats() {
  const [total, oldest] = await Promise.all([
    sql<{ count: number }[]>`SELECT COUNT(*)::INTEGER AS count FROM events`,
    sql<{ ts_ms: string | null }[]>`SELECT MIN(ts_ms)::TEXT AS ts_ms FROM events`,
  ]);
  return {
    total_events: total[0]?.count ?? 0,
    oldest_event_ms: oldest[0]?.ts_ms ? Number(oldest[0].ts_ms) : null,
  };
}
