use super::*;
use crate::services::{lesson_storage::LessonInfo, ta::Draft};
use std::path::Path;

const SCHEMA: &str = include_str!("store_sections.sql");

pub(super) async fn migrate(tx: &mut Transaction<'_, Sqlite>) -> Result<(), StoreError> {
    sqlx::raw_sql(SCHEMA).execute(&mut **tx).await?;
    let documents: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT name,payload FROM store_documents
         WHERE (name >= 'sidecar/course_data/' AND name < 'sidecar/course_data0' AND name LIKE '%/index.json')
            OR (name >= 'sidecar/./course_data/' AND name < 'sidecar/./course_data0' AND name LIKE '%/index.json')
            OR (name >= 'sidecar/ta_drafts/' AND name < 'sidecar/ta_drafts0' AND name LIKE '%.json')",
    ).fetch_all(&mut **tx).await?;
    for (scope, payload) in documents {
        let parts: Vec<_> = scope.split('/').collect();
        match parts.as_slice() {
            ["sidecar", "course_data", owner, course, "index.json"]
            | ["sidecar", ".", "course_data", owner, course, "index.json"]
                if !owner.is_empty() && !course.is_empty() =>
            {
                let key = scope.replacen("sidecar/course_data/", "sidecar/./course_data/", 1);
                import_lessons(tx, &key, payload.as_deref()).await?;
            }
            ["sidecar", "ta_drafts", user, _] if user.parse::<Uuid>().is_ok() => {
                import_drafts(tx, &scope, payload.as_deref()).await?;
            }
            _ => continue,
        }
        sqlx::query("UPDATE store_documents SET payload=NULL WHERE name=?")
            .bind(&scope)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn legacy(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    path: &Path,
) -> Result<Option<String>, StoreError> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT payload FROM store_documents WHERE name=?")
            .bind(scope)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(payload) = stored {
        return Ok(payload);
    }
    match tokio::fs::read_to_string(path).await {
        Ok(payload) => Ok(Some(payload)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn put_lessons(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    lessons: &[LessonInfo],
    payloads: impl IntoIterator<Item = String>,
) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::new();
    for lesson in lessons {
        if !seen.insert(&lesson.id) {
            return Err("课节身份重复，无法保存".into());
        }
    }
    let ids = serde_json::to_string(&seen)?;
    sqlx::query("DELETE FROM course_lessons WHERE scope=? AND lesson_id NOT IN (SELECT value FROM json_each(?))")
        .bind(scope).bind(ids).execute(&mut **tx).await?;
    for (position, (lesson, payload)) in lessons.iter().zip(payloads).enumerate() {
        sqlx::query(
            "INSERT INTO course_lessons(scope,lesson_id,position,payload) VALUES (?,?,?,?)
             ON CONFLICT(scope,lesson_id) DO UPDATE SET position=excluded.position,payload=excluded.payload
             WHERE course_lessons.position<>excluded.position OR course_lessons.payload<>excluded.payload",
        ).bind(scope).bind(&lesson.id).bind(position as i64).bind(payload).execute(&mut **tx).await?;
    }
    Ok(())
}

async fn import_lessons(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    payload: Option<&str>,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO lesson_indexes(scope) VALUES (?)")
        .bind(scope)
        .execute(&mut **tx)
        .await?;
    if let Some(payload) = payload {
        // Preserve the original JSON values so recovery can compare old journals.
        let lessons: Vec<LessonInfo> = serde_json::from_str(payload)?;
        let raw: Vec<serde_json::Value> = serde_json::from_str(payload)?;
        put_lessons(
            tx,
            scope,
            &lessons,
            raw.into_iter().map(|row| row.to_string()),
        )
        .await?;
    }
    Ok(())
}

async fn import_drafts(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    payload: Option<&str>,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO draft_indexes(scope) VALUES (?)")
        .bind(scope)
        .execute(&mut **tx)
        .await?;
    for draft in payload
        .map(serde_json::from_str::<Vec<Draft>>)
        .transpose()?
        .unwrap_or_default()
    {
        put_draft(tx, scope, &draft, true).await?;
    }
    Ok(())
}

async fn put_draft(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    draft: &Draft,
    overwrite: bool,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "INSERT INTO ta_drafts(scope,attempt_id,payload) VALUES (?,?,?)
         ON CONFLICT(scope,attempt_id) DO UPDATE SET payload=excluded.payload WHERE ?",
    )
    .bind(scope)
    .bind(&draft.attempt_id)
    .bind(serde_json::to_string(draft)?)
    .bind(overwrite)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() != 0)
}

impl FileStore {
    pub async fn import_legacy_section_files(&self) -> Result<(), StoreError> {
        let course_root = Path::new("./course_data");
        for account in child_directories(course_root).await? {
            for course in child_directories(&account).await? {
                let index = course.join("index.json");
                match tokio::fs::symlink_metadata(&index).await {
                    Ok(metadata) if metadata.is_file() => {
                        self.lesson_index_json(&index).await?;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        for account in child_directories(Path::new("ta_drafts")).await? {
            if account
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| name.parse::<Uuid>().is_err())
            {
                continue;
            }
            let mut entries = tokio::fs::read_dir(account).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if !entry.file_type().await?.is_file() {
                    continue;
                }
                let Some((course, column)) = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split_once('.'))
                else {
                    continue;
                };
                if path.extension().is_some_and(|ext| ext == "json")
                    && crate::services::ta::valid_id(course).is_ok()
                    && crate::services::ta::valid_id(column).is_ok()
                {
                    self.grading_drafts(&path).await?;
                }
            }
        }
        Ok(())
    }

    // None is distinct from a saved empty index when recovering a file journal.
    pub async fn lesson_index_json(&self, path: &Path) -> Result<Option<String>, StoreError> {
        let scope = Self::sidecar_key(path);
        let rows: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT l.payload FROM lesson_indexes i LEFT JOIN course_lessons l ON l.scope=i.scope
             WHERE i.scope=? ORDER BY l.position",
        )
        .bind(&scope)
        .fetch_all(&self.db)
        .await?;
        if !rows.is_empty() {
            return Ok(Some(format!(
                "[{}]",
                rows.into_iter().flatten().collect::<Vec<_>>().join(",")
            )));
        }

        let mut tx = self.begin_note_transaction().await?;
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM lesson_indexes WHERE scope=?)")
                .bind(&scope)
                .fetch_one(&mut *tx)
                .await?;
        if !known {
            let Some(payload) = legacy(&mut tx, &scope, path).await? else {
                tx.commit().await?;
                return Ok(None);
            };
            import_lessons(&mut tx, &scope, Some(&payload)).await?;
            Self::mask_document(&mut tx, path).await?;
        }
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT payload FROM course_lessons WHERE scope=? ORDER BY position",
        )
        .bind(&scope)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(format!("[{}]", rows.join(","))))
    }

    pub async fn save_lesson_index(
        &self,
        path: &Path,
        lessons: &[LessonInfo],
    ) -> Result<(), StoreError> {
        let scope = Self::sidecar_key(path);
        let payloads = lessons
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        let mut tx = self.begin_note_transaction().await?;
        let created = sqlx::query(
            "INSERT INTO lesson_indexes(scope) VALUES (?) ON CONFLICT(scope) DO NOTHING",
        )
        .bind(&scope)
        .execute(&mut *tx)
        .await?;
        put_lessons(&mut tx, &scope, lessons, payloads).await?;
        if created.rows_affected() > 0 {
            Self::mask_document(&mut tx, path).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn change_lesson(
        &self,
        path: &Path,
        id: &str,
        update: impl FnOnce(&mut LessonInfo),
    ) -> Result<bool, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let scope = Self::sidecar_key(path);
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM course_lessons WHERE scope=? AND lesson_id=?")
                .bind(&scope)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(payload) = payload else {
            return Ok(false);
        };
        let mut lesson = serde_json::from_str(&payload)?;
        update(&mut lesson);
        sqlx::query("UPDATE course_lessons SET payload=? WHERE scope=? AND lesson_id=?")
            .bind(serde_json::to_string(&lesson)?)
            .bind(&scope)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn draft_transaction(
        &self,
        path: &Path,
        scope: &str,
    ) -> Result<Transaction<'_, Sqlite>, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_indexes WHERE scope=?)")
                .bind(scope)
                .fetch_one(&mut *tx)
                .await?;
        if !known {
            let payload = legacy(&mut tx, scope, path).await?;
            import_drafts(&mut tx, scope, payload.as_deref()).await?;
            Self::mask_document(&mut tx, path).await?;
        }
        Ok(tx)
    }

    pub async fn has_grading_draft(&self, path: &Path, attempt: &str) -> Result<bool, StoreError> {
        let scope = Self::sidecar_key(path);
        let found: Option<bool> = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ta_drafts WHERE scope=?1 AND attempt_id=?2)
             FROM draft_indexes WHERE scope=?1",
        )
        .bind(&scope)
        .bind(attempt)
        .fetch_optional(&self.db)
        .await?;
        if let Some(found) = found {
            return Ok(found);
        }

        let mut tx = self.draft_transaction(path, &scope).await?;
        let exists = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ta_drafts WHERE scope=? AND attempt_id=?)",
        )
        .bind(&scope)
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(exists)
    }

    pub async fn grading_drafts(&self, path: &Path) -> Result<Vec<Draft>, StoreError> {
        let scope = Self::sidecar_key(path);
        let rows: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT d.payload FROM draft_indexes i LEFT JOIN ta_drafts d ON d.scope=i.scope
             WHERE i.scope=? ORDER BY d.rowid",
        )
        .bind(&scope)
        .fetch_all(&self.db)
        .await?;
        if !rows.is_empty() {
            return Ok(rows
                .into_iter()
                .flatten()
                .map(|row| serde_json::from_str(&row))
                .collect::<Result<_, _>>()?);
        }

        let mut tx = self.draft_transaction(path, &scope).await?;
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT payload FROM ta_drafts WHERE scope=? ORDER BY rowid")
                .bind(scope)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|row| serde_json::from_str(row))
            .collect::<Result<_, _>>()?)
    }

    pub async fn save_grading_drafts(
        &self,
        path: &Path,
        drafts: &[Draft],
        overwrite: bool,
    ) -> Result<usize, StoreError> {
        let scope = Self::sidecar_key(path);
        let mut tx = self.draft_transaction(path, &scope).await?;
        let mut saved = 0;
        for draft in drafts {
            saved += usize::from(put_draft(&mut tx, &scope, draft, overwrite).await?);
        }
        tx.commit().await?;
        Ok(saved)
    }

    pub async fn discard_grading_drafts(
        &self,
        path: &Path,
        selected: &std::collections::HashSet<&str>,
    ) -> Result<(), StoreError> {
        let scope = Self::sidecar_key(path);
        let mut tx = self.draft_transaction(path, &scope).await?;
        sqlx::query("DELETE FROM ta_drafts WHERE scope=? AND synced=0 AND attempt_id IN (SELECT value FROM json_each(?))")
            .bind(scope).bind(serde_json::to_string(selected)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn child_directories(path: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut directories = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && !entry.file_name().to_string_lossy().starts_with('.')
        {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn version_one_documents_migrate_once_and_mutate_individual_records() {
        let temp = tempdir::TempDir::new_in("target", "sections-v1").unwrap();
        let lesson_path = Path::new("./course_data/student/course/index.json");
        let user = Uuid::new_v4();
        let draft_path = PathBuf::from("ta_drafts")
            .join(user.to_string())
            .join("_course._column.json");
        let old = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(temp.path().join("assistant.sqlite3"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("store_schema.sql"))
            .execute(&old)
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE store_migrations(version INTEGER PRIMARY KEY); INSERT INTO store_migrations VALUES (1);")
            .execute(&old).await.unwrap();
        let lesson = |id| {
            serde_json::json!({
                "id":id,"video_url":"","play_url":"","date":"2026-11-20","time":"08:00",
                "day_of_week":"","is_processed":false,"tex_filename":null
            })
        };
        let draft = |id, synced| {
            serde_json::json!({
                "attempt_id":id,"student_id":"student","score":7,"feedback":"old",
                "revision":"one","updated_at":"2026-11-20T00:00:00Z","synced":synced
            })
        };
        for (name, value) in [
            (
                FileStore::sidecar_key(lesson_path),
                serde_json::json!([lesson("second"), lesson("first")]),
            ),
            (
                FileStore::sidecar_key(&draft_path),
                serde_json::json!([draft("one", false), draft("two", true)]),
            ),
        ] {
            sqlx::query("INSERT INTO store_documents VALUES (?,?)")
                .bind(name)
                .bind(value.to_string())
                .execute(&old)
                .await
                .unwrap();
        }
        old.close().await;
        let store = FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(store.verify_database().await.unwrap()["schema_version"], 2);
        let scoped = FileStore::sidecar_key(lesson_path);
        let index = store.lesson_index_json(lesson_path).await.unwrap().unwrap();
        let mut lessons: Vec<LessonInfo> = serde_json::from_str(&index).unwrap();
        assert_eq!(lessons[0].id, "second");
        assert_eq!(
            store
                .sidecar_paths_under(Path::new("./course_data"))
                .await
                .unwrap(),
            ["student/course/index.json"]
        );
        lessons.remove(0);
        store
            .save_lesson_index(lesson_path, &lessons)
            .await
            .unwrap();
        let draft = store.grading_drafts(&draft_path).await.unwrap().remove(0);
        assert_eq!(
            store
                .save_grading_drafts(&draft_path, &[draft], false)
                .await
                .unwrap(),
            0
        );
        store
            .discard_grading_drafts(
                &draft_path,
                &std::collections::HashSet::from(["one", "two"]),
            )
            .await
            .unwrap();
        drop(store);
        let store = FileStore::new(temp.path().into()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM course_lessons WHERE scope=?")
            .bind(scoped)
            .fetch_one(&store.db)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let drafts = store.grading_drafts(&draft_path).await.unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].attempt_id, "two");
        assert!(drafts[0].synced);
    }
}
