use super::*;

impl FileStore {
    pub async fn get_conversations(&self, user_id: Uuid) -> Vec<FileConversation> {
        self.read_entities(
            "SELECT payload FROM conversations WHERE owner=? ORDER BY updated_at DESC,id DESC",
            Some(user_id.to_string()),
        )
        .await
    }

    pub async fn get_conversation(&self, id: Uuid) -> Option<FileConversation> {
        read_entity(&self.db, "conversations", id)
            .await
            .ok()
            .flatten()
    }

    pub async fn insert_conversation(&self, record: FileConversation) -> Result<(), StoreError> {
        self.update_conversation(record).await
    }

    pub async fn update_conversation(&self, record: FileConversation) -> Result<(), StoreError> {
        write_entity(
            &mut *self.db.acquire().await?,
            "conversations",
            record.id,
            &record,
        )
        .await
    }

    pub async fn get_messages(&self, conversation: Uuid) -> Vec<FileConversationMessage> {
        self.read_entities(
            "SELECT payload FROM messages WHERE owner=? ORDER BY created_at,id",
            Some(conversation.to_string()),
        )
        .await
    }

    pub async fn insert_message(&self, record: FileConversationMessage) -> Result<(), StoreError> {
        write_entity(
            &mut *self.db.acquire().await?,
            "messages",
            record.id,
            &record,
        )
        .await
    }

    pub async fn get_wechat_binding(&self, owner: Uuid) -> Option<FileUserWeChatBinding> {
        let rows: Vec<FileUserWeChatBinding> = self.read_entities(
            "SELECT payload FROM wechat_bindings WHERE owner=? ORDER BY updated_at DESC,id DESC LIMIT 1", Some(owner.to_string())).await;
        rows.into_iter().next()
    }

    pub async fn upsert_wechat_binding(
        &self,
        record: FileUserWeChatBinding,
    ) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO wechat_bindings(id,payload) VALUES (?,?) ON CONFLICT(owner,openid) DO UPDATE SET id=excluded.id,payload=excluded.payload")
            .bind(record.id.to_string()).bind(serde_json::to_string(&record)?).execute(&self.db).await?;
        Ok(())
    }

    pub async fn delete_wechat_binding(&self, owner: Uuid) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM wechat_bindings WHERE owner=?")
            .bind(owner.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn get_wechat_channel(&self, owner: Uuid) -> Option<FileUserWeChatChannel> {
        read_entity(&self.db, "wechat_channels", owner)
            .await
            .ok()
            .flatten()
    }

    pub async fn upsert_wechat_channel(
        &self,
        record: FileUserWeChatChannel,
    ) -> Result<(), StoreError> {
        write_entity(
            &mut *self.db.acquire().await?,
            "wechat_channels",
            record.user_id,
            &record,
        )
        .await
    }

    pub async fn get_all_wechat_channels(&self) -> Vec<FileUserWeChatChannel> {
        self.read_entities("SELECT payload FROM wechat_channels", None)
            .await
    }

    pub async fn get_oauth_state(&self, id: &str) -> Option<(Uuid, DateTime<Utc>)> {
        let record: FileWeChatOAuthState =
            read_entity(&self.db, "oauth_states", id).await.ok()??;
        (record.expires_at > Utc::now()).then_some((record.user_id, record.expires_at))
    }

    pub async fn insert_oauth_state(&self, record: FileWeChatOAuthState) -> Result<(), StoreError> {
        write_entity(
            &mut *self.db.acquire().await?,
            "oauth_states",
            &record.state,
            &record,
        )
        .await
    }

    pub async fn delete_oauth_state(&self, id: &str) -> Result<(), StoreError> {
        self.delete_entity("oauth_states", id).await
    }
}
