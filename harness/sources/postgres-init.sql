-- A schema with keys, so the catalog's key read is proven on real ones
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
-- A few rows and an ANALYZE, so pg_class carries a row estimate: a table
-- never analyzed reports -1, which the catalog rightly reads as none.
INSERT INTO shop.customers VALUES (1, 'Ann', NULL), (2, 'Bo', 'vip');
INSERT INTO shop.orders VALUES (1, 1, DATE '2026-03-01', 10.50), (2, 2, DATE '2026-03-02', 4.25), (3, 1, DATE '2026-03-03', 7.00);
ANALYZE shop.customers;
ANALYZE shop.orders;
