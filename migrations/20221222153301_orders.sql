CREATE TABLE IF NOT EXISTS orders (
  id integer primary key autoincrement not null,
  kind varchar(4) not null,
  event_id char(64) not null,
  event_kind integer not null,
  hash char(64),
  preimage char(64),
  buyer_pubkey char(64),
  seller_pubkey char(64),
  status char(10) not null,
  description varchar(1000) not null,
  payment_method varchar(500) not null,
  amount integer not null,
  fiat_code varchar(5) not null,
  fiat_amount integer not null,
  buyer_invoice text,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP not null
);