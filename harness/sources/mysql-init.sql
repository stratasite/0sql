-- The same small schema with keys for MySQL; the container runs this once
-- against MYSQL_DATABASE on its first start.
CREATE TABLE customers (
  id    INT PRIMARY KEY,
  name  VARCHAR(80) NOT NULL COMMENT 'As they sign',
  notes TEXT
) COMMENT 'Who buys';
CREATE TABLE orders (
  id          INT PRIMARY KEY,
  customer_id INT NOT NULL,
  placed      DATE NOT NULL,
  total       DECIMAL(12, 2) NOT NULL,
  CONSTRAINT orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id)
);
CREATE VIEW recent AS SELECT id, customer_id FROM orders WHERE placed > '2026-01-01';
