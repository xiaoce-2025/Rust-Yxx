CREATE TABLE lesson_indexes (
    scope TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE course_lessons (
    scope TEXT NOT NULL REFERENCES lesson_indexes(scope),
    lesson_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    PRIMARY KEY(scope, lesson_id)
);
CREATE INDEX course_lessons_order ON course_lessons(scope, position);

CREATE TABLE draft_indexes (
    scope TEXT PRIMARY KEY NOT NULL
);
CREATE TABLE ta_drafts (
    scope TEXT NOT NULL REFERENCES draft_indexes(scope),
    attempt_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    synced INTEGER GENERATED ALWAYS AS (coalesce(json_extract(payload,'$.synced'),0)) STORED,
    PRIMARY KEY(scope, attempt_id)
);

CREATE INDEX pkuvideo_jobs_admin_order ON pkuvideo_jobs(json_extract(payload,'$.updated_at') DESC, id);
CREATE INDEX pkuvideo_fw_jobs_admin_order ON pkuvideo_fw_jobs(json_extract(payload,'$.updated_at') DESC, id);
CREATE INDEX translation_jobs_admin_order ON translation_jobs(updated_at DESC, id);
CREATE INDEX users_admin_order ON users(json_extract(payload,'$.created_at') DESC, id);
