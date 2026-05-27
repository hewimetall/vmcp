-- Seed data for the public demo Postgres MCP behind mcp.runmcp.ru.
-- Loaded once by the postgres:16-alpine image on first boot
-- (mounted at /docker-entrypoint-initdb.d/init.sql by compose).
--
-- Small but JOIN-rich schema so the demo can show off
-- @modelcontextprotocol/server-postgres doing real SQL — not just SELECT 1.

\connect demo;

-- ---------------------------------------------------------------------------
-- Schema
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS departments (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    city TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS employees (
    id            SERIAL PRIMARY KEY,
    full_name     TEXT NOT NULL,
    email         TEXT NOT NULL UNIQUE,
    department_id INT  NOT NULL REFERENCES departments(id),
    hired_on      DATE NOT NULL,
    salary_rub    INT  NOT NULL CHECK (salary_rub > 0)
);

CREATE TABLE IF NOT EXISTS customers (
    id      SERIAL PRIMARY KEY,
    name    TEXT NOT NULL,
    country TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS orders (
    id          SERIAL PRIMARY KEY,
    customer_id INT NOT NULL REFERENCES customers(id),
    employee_id INT NOT NULL REFERENCES employees(id),
    placed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    total_rub   INT NOT NULL CHECK (total_rub >= 0),
    status      TEXT NOT NULL CHECK (status IN ('pending','paid','shipped','cancelled'))
);

CREATE INDEX IF NOT EXISTS idx_employees_department ON employees(department_id);
CREATE INDEX IF NOT EXISTS idx_orders_customer      ON orders(customer_id);
CREATE INDEX IF NOT EXISTS idx_orders_employee      ON orders(employee_id);
CREATE INDEX IF NOT EXISTS idx_orders_placed_at     ON orders(placed_at);

-- ---------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------

INSERT INTO departments (name, city) VALUES
    ('Engineering',  'Novosibirsk'),
    ('Sales',        'Moscow'),
    ('Support',      'Saint Petersburg'),
    ('Marketing',    'Kazan')
ON CONFLICT (name) DO NOTHING;

INSERT INTO employees (full_name, email, department_id, hired_on, salary_rub) VALUES
    ('Анна Иванова',     'a.ivanova@runmcp.ru',  (SELECT id FROM departments WHERE name='Engineering'), '2022-03-14', 320000),
    ('Дмитрий Петров',   'd.petrov@runmcp.ru',   (SELECT id FROM departments WHERE name='Engineering'), '2021-09-01', 410000),
    ('Мария Сидорова',   'm.sidorova@runmcp.ru', (SELECT id FROM departments WHERE name='Sales'),       '2023-01-10', 270000),
    ('Игорь Кузнецов',   'i.kuznetsov@runmcp.ru',(SELECT id FROM departments WHERE name='Sales'),       '2024-06-20', 250000),
    ('Елена Смирнова',   'e.smirnova@runmcp.ru', (SELECT id FROM departments WHERE name='Support'),     '2020-11-05', 195000),
    ('Павел Морозов',    'p.morozov@runmcp.ru',  (SELECT id FROM departments WHERE name='Marketing'),   '2023-08-19', 285000)
ON CONFLICT (email) DO NOTHING;

INSERT INTO customers (name, country) VALUES
    ('ООО "Северный путь"',   'RU'),
    ('CodeFest LLC',          'RU'),
    ('Runiti Cloud',          'RU'),
    ('Alpha Robotics GmbH',   'DE'),
    ('Sakura Labs Inc.',      'JP')
ON CONFLICT DO NOTHING;

INSERT INTO orders (customer_id, employee_id, placed_at, total_rub, status) VALUES
    (1, 3, '2026-01-15 09:30+03', 142000, 'shipped'),
    (2, 4, '2026-02-02 11:15+03',  78500, 'paid'),
    (3, 3, '2026-02-18 14:42+03', 310000, 'shipped'),
    (4, 4, '2026-03-04 10:05+03', 525000, 'pending'),
    (5, 3, '2026-03-21 16:20+03', 198000, 'paid'),
    (1, 4, '2026-04-10 09:00+03',  64000, 'cancelled'),
    (2, 3, '2026-04-29 12:33+03', 220000, 'shipped'),
    (3, 4, '2026-05-05 13:48+03', 410000, 'paid'),
    (4, 3, '2026-05-12 17:01+03', 175000, 'pending'),
    (5, 4, '2026-05-20 08:50+03',  92500, 'shipped')
ON CONFLICT DO NOTHING;

-- ---------------------------------------------------------------------------
-- Read-only role the MCP server connects as.
-- ---------------------------------------------------------------------------

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'demo') THEN
        CREATE ROLE demo LOGIN PASSWORD 'demo';
    END IF;
END$$;

REVOKE pg_read_server_files FROM demo;
-- Defence-in-depth: even though vmcp's SQL guard blocks pg_read_file* at the gateway,
-- the role itself should not hold the grant.

GRANT CONNECT ON DATABASE demo TO demo;
GRANT USAGE  ON SCHEMA   public TO demo;
GRANT SELECT ON ALL TABLES    IN SCHEMA public TO demo;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO demo;
