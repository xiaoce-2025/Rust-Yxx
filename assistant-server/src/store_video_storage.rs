//! Standalone file slots are independent of course_data and immutable task history.
use super::{write_collection, FileStore, FileUser, StoreError};
use crate::services::pkuvideo::{PipelineStatus, PkuVideoJob};
use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

impl FileStore {
    pub async fn reserve_standalone_job(&self, job: PkuVideoJob) -> Result<(), StoreError> {
        if job.course_id.is_some() {
            return Err("COURSE_JOB".into());
        }
        let mut users = self.users.write().await;
        let mut jobs = self.pkuvideo_jobs.write().await;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM collections WHERE name='pkuvideo_jobs'")
                .fetch_one(&mut *tx)
                .await?;
        let mut persisted: HashMap<Uuid, PkuVideoJob> = serde_json::from_str(&payload)?;
        if persisted.contains_key(&job.id) {
            return Err("DUPLICATE_JOB".into());
        }
        if persisted
            .values()
            .filter(|j| j.user_id == job.user_id && j.is_stored_standalone())
            .count()
            >= 3
        {
            return Err("STANDALONE_STORAGE_FULL".into());
        }
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM collections WHERE name='users'")
                .fetch_one(&mut *tx)
                .await?;
        let mut updated_users: HashMap<Uuid, FileUser> = serde_json::from_str(&payload)?;
        let user = updated_users
            .get_mut(&job.user_id)
            .ok_or("OWNER_MISMATCH")?;
        if user.storage_quota <= 0 {
            return Err("STORAGE_QUOTA_EXCEEDED".into());
        }
        user.storage_quota -= 1;
        user.updated_at = Utc::now();
        persisted.insert(job.id, job.clone());
        write_collection(&mut tx, "users", &serde_json::to_string(&updated_users)?).await?;
        write_collection(
            &mut tx,
            "pkuvideo_jobs",
            &serde_json::to_string(&persisted)?,
        )
        .await?;
        tx.commit().await?;
        *users = updated_users;
        *jobs = persisted;
        Ok(())
    }

    pub async fn archive_standalone_job(
        &self,
        id: Uuid,
        owner: Uuid,
    ) -> Result<PkuVideoJob, StoreError> {
        let mut users = self.users.write().await;
        let mut jobs = self.pkuvideo_jobs.write().await;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM collections WHERE name='pkuvideo_jobs'")
                .fetch_one(&mut *tx)
                .await?;
        let mut persisted: HashMap<Uuid, PkuVideoJob> = serde_json::from_str(&payload)?;
        let job = persisted.get_mut(&id).ok_or("JOB_NOT_FOUND")?;
        if job.user_id != owner {
            return Err("OWNER_MISMATCH".into());
        }
        if job.course_id.is_some() {
            return Err("COURSE_JOB".into());
        }
        if matches!(
            job.status,
            PipelineStatus::Pending | PipelineStatus::Running
        ) {
            return Err("ACTIVE_JOB".into());
        }
        if job.artifacts_deleted_at.is_some() {
            let job = job.clone();
            tx.commit().await?;
            return Ok(job);
        }
        job.artifacts_deleted_at = Some(Utc::now());
        job.notes_path = None;
        job.transcript_path = None;
        job.pdf_path = None;
        job.postprocessing = false;
        let result = job.clone();
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM collections WHERE name='users'")
                .fetch_one(&mut *tx)
                .await?;
        let mut updated_users: HashMap<Uuid, FileUser> = serde_json::from_str(&payload)?;
        let user = updated_users.get_mut(&owner).ok_or("OWNER_MISMATCH")?;
        user.storage_quota = user.storage_quota.checked_add(1).ok_or("StorageOverflow")?;
        user.updated_at = Utc::now();
        write_collection(&mut tx, "users", &serde_json::to_string(&updated_users)?).await?;
        write_collection(
            &mut tx,
            "pkuvideo_jobs",
            &serde_json::to_string(&persisted)?,
        )
        .await?;
        tx.commit().await?;
        *users = updated_users;
        *jobs = persisted;
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    async fn fixture() -> (tempdir::TempDir, Arc<FileStore>, Uuid) {
        let temp = tempdir::TempDir::new("standalone-slots").unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let id = Uuid::new_v4();
        store
            .insert_user(FileUser {
                id,
                username: id.to_string(),
                email: format!("{id}@example.invalid"),
                password_hash: String::new(),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                storage_quota: 10,
                processing_quota: 10,
                processing_priority: 5,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
            })
            .await
            .unwrap();
        (temp, store, id)
    }
    fn job(user: Uuid) -> PkuVideoJob {
        PkuVideoJob::new(
            Uuid::new_v4(),
            user,
            "https://example.invalid/video".into(),
            "unused".into(),
        )
    }
    #[tokio::test]
    async fn concurrent_submissions_cannot_exceed_three_and_courses_do_not_count() {
        let (_temp, store, user) = fixture().await;
        let mut course = job(user);
        course.course_id = Some("course".into());
        store.insert_pkuvideo_job(course).await.unwrap();
        let (a, b, c, d) = tokio::join!(
            store.reserve_standalone_job(job(user)),
            store.reserve_standalone_job(job(user)),
            store.reserve_standalone_job(job(user)),
            store.reserve_standalone_job(job(user))
        );
        assert_eq!([a, b, c, d].into_iter().filter(Result::is_ok).count(), 3);
        assert_eq!(
            store
                .get_all_pkuvideo_jobs()
                .await
                .values()
                .filter(|j| j.is_stored_standalone())
                .count(),
            3
        );
        assert_eq!(store.get_user(user).await.unwrap().storage_quota, 7);
        // A fresh store also observes the same durable limit.
        let reopened = FileStore::new(_temp.path().into()).await.unwrap();
        assert!(reopened.reserve_standalone_job(job(user)).await.is_err());
    }
    #[tokio::test]
    async fn deletion_preserves_charge_and_history_and_releases_one_slot_once() {
        let (_temp, store, user) = fixture().await;
        let a = job(user);
        store.reserve_standalone_job(a.clone()).await.unwrap();
        store.queue_note_billing(a.id, user).await.unwrap();
        assert!(store.check_note_start(a.id, user).await.unwrap());
        store.charge_note_job(a.clone(), 3601).await.unwrap();
        for _ in 0..3 {
            store.archive_standalone_job(a.id, user).await.unwrap();
        }
        assert_eq!(store.get_user(user).await.unwrap().storage_quota, 10);
        let archived = store.get_pkuvideo_job(a.id).await.unwrap();
        assert_eq!(archived.status, PipelineStatus::Succeeded);
        assert_eq!(archived.charged_units, 2);
        assert!(!archived.is_stored_standalone());
        assert!(archived.artifacts_deleted_at.is_some());
        assert_eq!(store.note_balance(user).await.unwrap(), 8);
        assert_eq!(
            store.note_ledger(user, 30, None).await.unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let reopened = FileStore::new(_temp.path().into()).await.unwrap();
        assert!(reopened
            .get_pkuvideo_job(a.id)
            .await
            .unwrap()
            .artifacts_deleted_at
            .is_some());
    }
    #[tokio::test]
    async fn failed_jobs_release_storage_but_active_course_and_foreign_jobs_are_protected() {
        let (_temp, store, user) = fixture().await;
        let mut a = job(user);
        store.reserve_standalone_job(a.clone()).await.unwrap();
        assert!(store.archive_standalone_job(a.id, user).await.is_err());
        a.status = PipelineStatus::Failed;
        store.update_pkuvideo_job(a.clone()).await.unwrap();
        assert!(store
            .archive_standalone_job(a.id, Uuid::new_v4())
            .await
            .is_err());
        store.archive_standalone_job(a.id, user).await.unwrap();
        assert_eq!(store.get_user(user).await.unwrap().storage_quota, 10);
        let mut course = job(user);
        course.course_id = Some("course".into());
        course.status = PipelineStatus::Succeeded;
        store.insert_pkuvideo_job(course.clone()).await.unwrap();
        assert!(store.archive_standalone_job(course.id, user).await.is_err());
        assert_eq!(store.get_user(user).await.unwrap().storage_quota, 10);
    }
}
