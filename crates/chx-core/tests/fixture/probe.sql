CREATE TABLE probe (
    id UInt64,
    venue LowCardinality(String),
    px Decimal(38, 18),
    ts DateTime64(9, 'UTC'),
    tags Array(String),
    note Nullable(String),
    kind Enum8('buy' = 1, 'sell' = 2)
) ENGINE = MergeTree ORDER BY id
