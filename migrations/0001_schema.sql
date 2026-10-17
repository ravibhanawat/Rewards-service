-- Money is always integer paise (1 INR = 100 paise) stored as BIGINT.
-- Constraints below are the last line of defence for the invariants in
-- DECISIONS.md; the application enforces the same rules first so it can
-- return precise errors, but a bug in application code cannot violate them.

CREATE TABLE products (
    id           UUID PRIMARY KEY,
    name         TEXT        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    price_paise  BIGINT      NOT NULL CHECK (price_paise >= 0),
    -- Inventory can never go negative, whatever the application does.
    stock        INTEGER     NOT NULL CONSTRAINT products_stock_non_negative CHECK (stock >= 0),
    -- Discontinued products stay in the table so historical orders keep a
    -- valid reference; they simply cannot be added to carts or checked out.
    active       BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE carts (
    id          UUID PRIMARY KEY,
    status      TEXT        NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'checked_out')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE cart_items (
    cart_id     UUID        NOT NULL REFERENCES carts (id),
    product_id  UUID        NOT NULL REFERENCES products (id),
    quantity    INTEGER     NOT NULL CHECK (quantity BETWEEN 1 AND 1000),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One line per product per cart.
    PRIMARY KEY (cart_id, product_id)
);

-- A coupon has no mutable status column. It is "redeemed" exactly when an
-- order references it (orders.coupon_id is UNIQUE), so coupon state can never
-- disagree with the orders that consumed it.
CREATE TABLE coupons (
    id                     UUID PRIMARY KEY,
    code                   TEXT        NOT NULL CONSTRAINT coupons_code_unique UNIQUE,
    -- The successful-order count that earned this coupon (n, 2n, 3n, ...).
    -- UNIQUE makes a second coupon for the same milestone impossible.
    milestone_order_count  BIGINT      NOT NULL CONSTRAINT coupons_milestone_unique UNIQUE
                                       CHECK (milestone_order_count > 0),
    discount_percent       INTEGER     NOT NULL CHECK (discount_percent BETWEEN 1 AND 100),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orders (
    id                   UUID PRIMARY KEY,
    -- A cart produces at most one order.
    cart_id              UUID        NOT NULL CONSTRAINT orders_cart_unique UNIQUE REFERENCES carts (id),
    -- The order row doubles as the idempotency record: it commits atomically
    -- with the inventory decrement, so the two can never disagree.
    idempotency_key      TEXT        NOT NULL CONSTRAINT orders_idempotency_key_unique UNIQUE,
    request_fingerprint  TEXT        NOT NULL,
    subtotal_paise       BIGINT      NOT NULL CHECK (subtotal_paise >= 0),
    discount_paise       BIGINT      NOT NULL CHECK (discount_paise >= 0),
    total_paise          BIGINT      NOT NULL CHECK (total_paise >= 0),
    -- A coupon is redeemed by at most one order.
    coupon_id            UUID        CONSTRAINT orders_coupon_unique UNIQUE REFERENCES coupons (id),
    -- Snapshot of the coupon terms used, so the order explains its own total.
    coupon_code          TEXT,
    discount_percent     INTEGER     CHECK (discount_percent BETWEEN 1 AND 100),
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT orders_discount_within_subtotal CHECK (discount_paise <= subtotal_paise),
    CONSTRAINT orders_total_reconciles CHECK (total_paise = subtotal_paise - discount_paise),
    CONSTRAINT orders_coupon_snapshot_complete CHECK (
        (coupon_id IS NULL AND coupon_code IS NULL AND discount_percent IS NULL AND discount_paise = 0)
        OR (coupon_id IS NOT NULL AND coupon_code IS NOT NULL AND discount_percent IS NOT NULL)
    )
);

-- Immutable purchase snapshot: name and price as charged, independent of
-- later product edits.
CREATE TABLE order_items (
    order_id          UUID    NOT NULL REFERENCES orders (id),
    product_id        UUID    NOT NULL REFERENCES products (id),
    product_name      TEXT    NOT NULL,
    unit_price_paise  BIGINT  NOT NULL CHECK (unit_price_paise >= 0),
    quantity          INTEGER NOT NULL CHECK (quantity > 0),
    line_total_paise  BIGINT  NOT NULL,
    PRIMARY KEY (order_id, product_id),
    CONSTRAINT order_items_line_total_reconciles CHECK (line_total_paise = unit_price_paise * quantity)
);

-- Orders, their items and coupons are append-only facts. Reports and coupon
-- status are derived from them, so forbidding UPDATE/DELETE keeps every
-- derived number reproducible.
CREATE FUNCTION reject_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% on % is not allowed: rows are immutable', TG_OP, TG_TABLE_NAME;
END;
$$;

CREATE TRIGGER orders_immutable BEFORE UPDATE OR DELETE ON orders
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();
CREATE TRIGGER order_items_immutable BEFORE UPDATE OR DELETE ON order_items
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();
CREATE TRIGGER coupons_immutable BEFORE UPDATE OR DELETE ON coupons
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();
