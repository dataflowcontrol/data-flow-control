SELECT category, sum(amount) AS total
FROM orders
GROUP BY category
