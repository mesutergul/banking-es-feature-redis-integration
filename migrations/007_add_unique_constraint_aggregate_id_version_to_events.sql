-- Drop the existing constraint that includes timestamp
--ALTER TABLE events DROP CONSTRAINT IF EXISTS unique_aggregate_version_timestamp;

-- Drop the redundant index since the unique constraint will create its own index
DROP INDEX IF EXISTS idx_events_aggregate_version;

-- First, create a new table with the desired partitioning strategy
CREATE TABLE events_new (
    id UUID NOT NULL,
    aggregate_id UUID NOT NULL,
    event_type VARCHAR(100) NOT NULL,
    event_data JSONB NOT NULL,
    version BIGINT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
) PARTITION BY HASH (aggregate_id);

-- Create partitions for the new table
CREATE TABLE events_new_p0 PARTITION OF events_new FOR VALUES WITH (modulus 4, remainder 0);
CREATE TABLE events_new_p1 PARTITION OF events_new FOR VALUES WITH (modulus 4, remainder 1);
CREATE TABLE events_new_p2 PARTITION OF events_new FOR VALUES WITH (modulus 4, remainder 2);
CREATE TABLE events_new_p3 PARTITION OF events_new FOR VALUES WITH (modulus 4, remainder 3);

-- Copy data from old table to new table
INSERT INTO events_new 
SELECT * FROM events;

-- Drop the old table
DROP TABLE events;

-- Rename the new table to the original name
ALTER TABLE events_new RENAME TO events;

-- Add the unique constraint
ALTER TABLE events ADD CONSTRAINT unique_aggregate_id_version 
    UNIQUE (aggregate_id, version);

-- Add comment explaining the constraint
COMMENT ON CONSTRAINT unique_aggregate_id_version ON events IS 'Ensures optimistic concurrency control by preventing duplicate versions for the same aggregate';

-- Recreate the necessary indexes
CREATE INDEX IF NOT EXISTS idx_events_timestamp
    ON events (timestamp)
    WITH (fillfactor = 90);

CREATE INDEX IF NOT EXISTS idx_events_event_type
    ON events (event_type)
    WITH (fillfactor = 90);

CREATE INDEX IF NOT EXISTS idx_events_data_gin
    ON events USING GIN (event_data jsonb_path_ops);