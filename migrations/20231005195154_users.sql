CREATE TABLE IF NOT EXISTS users (
  pubkey char(64) primary key not null,
  is_admin integer not null default 0,
  admin_password char(64) not null default '',
  is_solver integer not null default 0,
  is_banned integer not null default 0,
  category integer not null default 0,
  last_trade_index integer not null default 0,
  total_reviews integer not null default 0,
  total_rating real not null default 0.0,
  last_rating integer not null default 0,
  max_rating integer not null default 0,
  min_rating integer not null default 0,
  created_at integer not null
);