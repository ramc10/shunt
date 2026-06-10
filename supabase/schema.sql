-- shunt telemetry schema
-- Run this in the Supabase SQL editor after creating a project.
-- Then fill in SUPABASE_URL and SUPABASE_ANON_KEY in src/telemetry.rs.

-- One row per install. Upserted on daemon_start.
create table if not exists installs (
  id          text        primary key,
  first_seen  timestamptz not null default now(),
  last_seen   timestamptz not null default now(),
  version     text,
  os          text,
  arch        text
);

-- Time-series events.
create table if not exists events (
  id          bigserial   primary key,
  created_at  timestamptz not null default now(),
  install_id  text        not null,
  event_type  text        not null,
  payload     jsonb       not null default '{}'
);

create index if not exists events_install_id_idx  on events(install_id);
create index if not exists events_event_type_idx  on events(event_type);
create index if not exists events_created_at_idx  on events(created_at desc);

-- Enable realtime on both tables for the live dashboard.
alter publication supabase_realtime add table installs;
alter publication supabase_realtime add table events;

-- RLS: anon key can INSERT only. Service role bypasses RLS.
alter table installs enable row level security;
alter table events   enable row level security;

create policy "anon insert installs"
  on installs for insert to anon with check (true);

create policy "anon insert events"
  on events for insert to anon with check (true);
