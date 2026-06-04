-- Representative scan with REMOVE policy on orders (populate orders in harness).
SELECT id, amount FROM orders WHERE amount > 0
