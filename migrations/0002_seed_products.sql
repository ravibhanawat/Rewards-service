-- Fixed IDs so the README's curl examples work against a fresh database.
INSERT INTO products (id, name, price_paise, stock) VALUES
    ('00000000-0000-4000-8000-000000000001', 'Mechanical Keyboard',        349900,  50),
    ('00000000-0000-4000-8000-000000000002', 'Wireless Mouse',             129950, 100),
    ('00000000-0000-4000-8000-000000000003', 'USB-C Cable (1m)',            39900, 500),
    ('00000000-0000-4000-8000-000000000004', 'Laptop Stand',               189900,  25),
    -- Deliberately scarce: use this one to watch concurrent checkouts compete.
    ('00000000-0000-4000-8000-000000000005', 'Limited Edition Desk Mat',    99900,   3),
    ('00000000-0000-4000-8000-000000000006', 'Sticker Pack',                 4999, 200);
