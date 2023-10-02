CREATE TABLE IF NOT EXISTS disputes (
  order_id char(36) unique not null,
  status varchar(10) not null,
  solver_pubkey char(64),
  created_at integer not null,
  taken_at integer default 0
);