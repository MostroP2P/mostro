CREATE TABLE IF NOT EXISTS users (
  pubkey char(64) primary key not null,
  is_admin integer not null default 0,
  is_solver integer not null default 0,
  is_banned integer not null default 0,
  category integer not null default 0,
  created_at integer not null,
  trade_index integer not null default 0
);