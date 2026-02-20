use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataEntry {
    pub key: String,
    pub value: String,
}

#[allow(async_fn_in_trait)]
pub trait DataService: Send + Sync {
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    async fn get(&self, key: &str) -> anyhow::Result<Option<DataEntry>>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<DataEntry>>;
}
