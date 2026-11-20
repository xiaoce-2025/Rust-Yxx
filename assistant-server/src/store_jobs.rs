use super::*;

impl FileStore {
    pub async fn get_pkuvideo_job(&self, id: Uuid) -> Option<PkuVideoJob> {
        read_entity(&self.db, "pkuvideo_jobs", id)
            .await
            .ok()
            .flatten()
    }

    pub async fn get_pkuvideo_jobs_by_user(&self, owner: Uuid) -> Vec<PkuVideoJob> {
        self.read_entities(
            "SELECT payload FROM pkuvideo_jobs WHERE owner=? ORDER BY created_at DESC,id DESC",
            Some(owner.to_string()),
        )
        .await
    }

    pub async fn task_history_page(
        &self,
        owner: Uuid,
        search: &str,
        source: Option<bool>,
        status: Option<&str>,
        page: i64,
        size: i64,
    ) -> Result<(Vec<PkuVideoJob>, i64, i64, i64), StoreError> {
        let mut tx = self.db.begin().await?;
        let owner = owner.to_string();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM pkuvideo_jobs WHERE owner=?")
            .bind(&owner)
            .fetch_one(&mut *tx)
            .await?;
        let filter = " FROM pkuvideo_jobs WHERE owner=?1
            AND (?2 IS NULL OR (json_extract(payload,'$.course_id') IS NOT NULL)=?2)
            AND (?3 IS NULL OR status=?3)
            AND (?4='' OR instr(lower(id),?4)>0
                OR instr(lower(json_extract(payload,'$.course_name')),?4)>0
                OR instr(lower(json_extract(payload,'$.course_date')),?4)>0)";
        let count_sql = format!("SELECT count(*){filter}");
        let filtered: i64 = sqlx::query_scalar(&count_sql)
            .bind(&owner)
            .bind(source)
            .bind(status)
            .bind(search)
            .fetch_one(&mut *tx)
            .await?;
        let page = page.clamp(
            1,
            if filtered == 0 {
                1
            } else {
                (filtered - 1) / size + 1
            },
        );
        let select_sql =
            format!("SELECT payload{filter} ORDER BY created_at DESC,id DESC LIMIT ?5 OFFSET ?6");
        let records: Vec<String> = sqlx::query_scalar(&select_sql)
            .bind(&owner)
            .bind(source)
            .bind(status)
            .bind(search)
            .bind(size)
            .bind((page - 1) * size)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let jobs = records
            .iter()
            .map(|payload| serde_json::from_str(payload))
            .collect::<Result<_, _>>()?;
        Ok((jobs, total, filtered, page))
    }

    pub async fn standalone_jobs(&self, owner: Uuid) -> Vec<PkuVideoJob> {
        self.read_entities("SELECT payload FROM pkuvideo_jobs WHERE owner=? AND standalone=1 ORDER BY created_at DESC,id DESC", Some(owner.to_string())).await
    }

    pub async fn insert_pkuvideo_job(&self, job: PkuVideoJob) -> Result<(), StoreError> {
        self.update_pkuvideo_job(job).await
    }

    pub async fn update_pkuvideo_job(&self, job: PkuVideoJob) -> Result<(), StoreError> {
        self.persist_pkuvideo_job(&job).await?;
        Ok(())
    }

    pub(crate) async fn persist_pkuvideo_job(&self, job: &PkuVideoJob) -> Result<bool, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let charged: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM note_credit_ledger WHERE job_id=? AND event_type='JOB_CHARGE')")
            .bind(job.id.to_string()).fetch_one(&mut *tx).await?;
        if !charged {
            // A failure callback racing final settlement must not overwrite success.
            write_entity(&mut tx, "pkuvideo_jobs", job.id, job).await?;
        }
        tx.commit().await?;
        Ok(!charged)
    }

    pub async fn delete_pkuvideo_job(&self, id: Uuid) -> Result<(), StoreError> {
        self.delete_entity("pkuvideo_jobs", id).await
    }

    pub async fn get_all_pkuvideo_jobs(&self) -> Result<HashMap<Uuid, PkuVideoJob>, StoreError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM pkuvideo_jobs")
            .fetch_all(&self.db)
            .await?;
        rows.iter()
            .map(|payload| {
                let job: PkuVideoJob = serde_json::from_str(payload)?;
                Ok((job.id, job))
            })
            .collect()
    }

    pub async fn insert_pkuvideo_fw_job(&self, job: PkuVideoFwJob) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO pkuvideo_fw_jobs(id,payload) VALUES (?,?)")
            .bind(job.id.to_string())
            .bind(serde_json::to_string(&job)?)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn update_pkuvideo_fw_job(&self, job: PkuVideoFwJob) -> Result<(), StoreError> {
        let result = sqlx::query("UPDATE pkuvideo_fw_jobs SET payload=? WHERE id=?")
            .bind(serde_json::to_string(&job)?)
            .bind(job.id.to_string())
            .execute(&self.db)
            .await?;
        if result.rows_affected() == 0 {
            return Err("回放任务不存在".into());
        }
        Ok(())
    }

    pub async fn delete_pkuvideo_fw_job(&self, id: Uuid) -> Result<(), StoreError> {
        self.delete_entity("pkuvideo_fw_jobs", id).await
    }

    pub async fn get_all_pkuvideo_fw_jobs(
        &self,
    ) -> Result<HashMap<Uuid, PkuVideoFwJob>, StoreError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM pkuvideo_fw_jobs")
            .fetch_all(&self.db)
            .await?;
        rows.iter()
            .map(|payload| {
                let job: PkuVideoFwJob = serde_json::from_str(payload)?;
                Ok((job.id, job))
            })
            .collect()
    }

    pub async fn save_translation_job(
        &self,
        job: &crate::services::video_translation::TranslationJob,
    ) -> Result<(), String> {
        let mut connection = self.db.acquire().await.map_err(|error| error.to_string())?;
        write_entity(&mut connection, "translation_jobs", job.id, job)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn translation_job_ids(&self) -> Result<Vec<Uuid>, String> {
        sqlx::query_scalar::<_, String>("SELECT id FROM translation_jobs")
            .fetch_all(&self.db)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|id| id.parse::<Uuid>().map_err(|error| error.to_string()))
            .collect()
    }

    pub async fn load_translation_job(
        &self,
        id: Uuid,
        legacy: &std::path::Path,
    ) -> Result<Option<crate::services::video_translation::TranslationJob>, String> {
        let job = read_entity(&self.db, "translation_jobs", id)
            .await
            .map_err(|error| error.to_string())?;
        if job.is_some() {
            return Ok(job);
        }
        let job: Option<crate::services::video_translation::TranslationJob> =
            self.load_sidecar(legacy).await?;
        if let Some(job) = job.as_ref().filter(|job| job.id == id && job.version == 1) {
            let mut tx = self
                .begin_note_transaction()
                .await
                .map_err(|error| error.to_string())?;
            write_entity(&mut tx, "translation_jobs", id, job)
                .await
                .map_err(|error| error.to_string())?;
            Self::mask_document(&mut tx, legacy).await?;
            tx.commit().await.map_err(|error| error.to_string())?;
        }
        Ok(job)
    }

    pub async fn delete_translation_job(
        &self,
        id: Uuid,
        legacy: &std::path::Path,
    ) -> Result<(), String> {
        let mut tx = self
            .begin_note_transaction()
            .await
            .map_err(|error| error.to_string())?;
        sqlx::query("DELETE FROM translation_jobs WHERE id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
        Self::mask_document(&mut tx, legacy).await?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}
