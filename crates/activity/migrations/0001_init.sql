-- Время везде — секунды Unix (UTC): сравнение периодов и ретенция не
-- зависят от часового пояса демона.

-- Проекты — git-репозитории под корнями наблюдения. Исчезнувший проект не
-- удаляется, а помечается `removed_at`: его события остаются в сводках.
CREATE TABLE projects (
    id            INTEGER PRIMARY KEY,
    path          TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    head_branch   TEXT,
    -- Снимок незакоммиченного на момент последнего опроса: число путей из
    -- `git status --porcelain` и первые из них (JSON-массив).
    dirty_files   INTEGER NOT NULL DEFAULT 0,
    dirty_sample  TEXT    NOT NULL DEFAULT '[]',
    first_seen    INTEGER NOT NULL,
    last_scanned  INTEGER,
    last_activity INTEGER,
    removed_at    INTEGER
);

-- Курсоры: последний увиденный коммит каждой локальной ветки. Новые коммиты
-- — это то, что достижимо от текущей вершины и недостижимо от курсоров.
CREATE TABLE refs (
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    sha        TEXT    NOT NULL,
    PRIMARY KEY (project_id, name)
);

-- Журнал изменений. `seen_at` — когда демон заметил событие, а не дата
-- коммита: коммит, подтянутый из чужой ветки, может быть датирован прошлым
-- месяцем, но в сводку попадает тот период, когда он появился локально.
CREATE TABLE events (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL,
    seen_at    INTEGER NOT NULL,
    sha        TEXT,
    payload    TEXT    NOT NULL
);

-- Один коммит в проекте — одно событие, даже если он появился в нескольких
-- ветках (слияние feature-ветки в main).
CREATE UNIQUE INDEX events_commit ON events (project_id, sha) WHERE sha IS NOT NULL;
CREATE INDEX events_seen ON events (seen_at);

-- Готовые сводки. `acked_at IS NULL` — клиент её ещё не показал.
CREATE TABLE digests (
    id          INTEGER PRIMARY KEY,
    period_from INTEGER NOT NULL,
    period_to   INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    body        TEXT    NOT NULL,
    text        TEXT    NOT NULL,
    acked_at    INTEGER
);

CREATE INDEX digests_period ON digests (period_to);
