CREATE TABLE IF NOT EXISTS users (
  id char(36) primary key not null,
  pubkey char(64) unique not null,
  is_admin integer not null default 0,
  is_solver integer not null default 0,
  is_banned integer not null default 0,
  category integer not null default 0,
  last_trade_index integer not null default 0,
  total_reviews integer not null default 0,
  total_rating integer not null default 0,
  last_rating integer not null default 0,
  max_rating integer not null default 0,
  min_rating integer not null default 0,
  created_at integer not null
);