CREATE TABLE IF NOT EXISTS disputes (
  id char(36) primary key not null,
  order_id char(36) unique not null,
  status varchar(10) not null,
  order_previous_status varchar(10) not null,
  solver_pubkey char(64),
  created_at integer not null,
  taken_at integer default 0,
  buyer_token integer not null,
  seller_token integer not null
);