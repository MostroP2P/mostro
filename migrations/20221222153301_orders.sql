CREATE TABLE IF NOT EXISTS orders (
  id blob primary key not null,
  kind varchar(4) not null,
  event_id char(64) not null,
  hash char(64),
  preimage char(64),
  buyer_pubkey char(64),
  seller_pubkey char(64),
  status char(10) not null,
  prime integer not null,
  payment_method varchar(500) not null,
  amount integer not null,
  fiat_code varchar(5) not null,
  fiat_amount integer not null,
  buyer_invoice text,
  created_at integer not null
);