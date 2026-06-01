SELECT orders.id, customers.id
FROM orders
JOIN customers ON orders.id = customers.id
