-- A schema with keys, so the catalogue's key read is proven on real ones
-- rather than on an empty answer. Runs once, on the container's first
-- start, from docker-entrypoint-initdb.d.
CREATE SCHEMA shop;
CREATE TABLE shop.customers (
  id    integer PRIMARY KEY,
  name  varchar(80) NOT NULL,
  notes text
);
COMMENT ON TABLE shop.customers IS 'Who buys';
COMMENT ON COLUMN shop.customers.name IS 'As they sign';
CREATE TABLE shop.orders (
  id          integer PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES shop.customers (id),
  placed      date NOT NULL,
  total       numeric(12, 2) NOT NULL
);
CREATE VIEW shop.recent AS SELECT id, customer_id FROM shop.orders WHERE placed > DATE '2026-01-01';
