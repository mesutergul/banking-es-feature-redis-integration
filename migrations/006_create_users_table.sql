-- Migration to create the users table for storing authentication and user information

-- Ensure uuid-ossp extension is available (though it should be from 001_create_events_table.sql)
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    username TEXT UNIQUE NOT NULL,
    email TEXT UNIQUE, -- Nullable, but unique if provided
    password_hash TEXT NOT NULL,
    roles TEXT[] NOT NULL DEFAULT '{}', -- Array of roles (e.g., admin, user)
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ,
    failed_login_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until TIMESTAMPTZ
);

-- Add comments to explain the purpose of columns
COMMENT ON COLUMN users.id IS 'Unique identifier for the user';
COMMENT ON COLUMN users.username IS 'User''s login name, must be unique';
COMMENT ON COLUMN users.email IS 'User''s email address, unique if provided';
COMMENT ON COLUMN users.password_hash IS 'Hashed password (e.g., using Argon2)';
COMMENT ON COLUMN users.roles IS 'Array of roles assigned to the user';
COMMENT ON COLUMN users.is_active IS 'Flag indicating if the user account is active';
COMMENT ON COLUMN users.registered_at IS 'Timestamp of when the user account was created';
COMMENT ON COLUMN users.last_login_at IS 'Timestamp of the user''s last successful login';
COMMENT ON COLUMN users.failed_login_attempts IS 'Counter for consecutive failed login attempts';
COMMENT ON COLUMN users.locked_until IS 'If account is locked, timestamp when the lock expires';

-- Create indexes for frequently queried columns
CREATE INDEX IF NOT EXISTS idx_users_username ON users (username);
-- The UNIQUE constraint on username and email already creates indexes,
-- but explicit index creation doesn't hurt and documents intent.
-- For roles, if querying by role becomes common:
-- CREATE INDEX IF NOT EXISTS idx_users_roles_gin ON users USING GIN (roles);