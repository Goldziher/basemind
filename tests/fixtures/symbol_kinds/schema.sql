CREATE SCHEMA app;

CREATE TABLE IF NOT EXISTS public.users (
    id int PRIMARY KEY,
    org_id int REFERENCES orgs (id),
    name text
);

CREATE TYPE mood AS ENUM ('happy', 'sad');

CREATE VIEW active_users AS
SELECT * FROM users WHERE name IS NOT NULL;

CREATE MATERIALIZED VIEW user_counts AS
SELECT count(*) FROM users;

CREATE OR REPLACE FUNCTION public.add(a int, b int) RETURNS int AS $$
BEGIN
    RETURN a + b;
END;
$$ LANGUAGE plpgsql;

CREATE PROCEDURE purge() LANGUAGE sql AS $$ DELETE FROM users $$;

CREATE UNIQUE INDEX idx_users_name ON users (name);

CREATE INDEX ON users (org_id);

CREATE SEQUENCE user_ids;

CREATE TRIGGER audit_users BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION add();

SELECT * FROM users;
