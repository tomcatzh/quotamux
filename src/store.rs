use std::{fs, path::Path, sync::Arc};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Serialize, de::DeserializeOwned};

use crate::types::{AlertRecord, AttemptRecord, RequestRecord};

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("requests");
const ATTEMPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("attempts");
const ALERTS: TableDefinition<&str, &[u8]> = TableDefinition::new("alerts");

#[derive(Clone)]
pub struct Store {
    database: Arc<Database>,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir).map_err(|error| error.to_string())?;
        let database =
            Database::create(data_dir.join("quotamux.redb")).map_err(|error| error.to_string())?;
        let store = Self {
            database: Arc::new(database),
        };
        let transaction = store
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(STATE)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(REQUESTS)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(ATTEMPTS)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(ALERTS)
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(store)
    }

    pub fn put_state<T: Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        self.put(STATE, key, value)
    }

    pub fn get_state<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, String> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let table = transaction
            .open_table(STATE)
            .map_err(|error| error.to_string())?;
        let Some(value) = table.get(key).map_err(|error| error.to_string())? else {
            return Ok(None);
        };
        serde_json::from_slice(value.value())
            .map(Some)
            .map_err(|error| error.to_string())
    }

    pub fn record_request(&self, record: &RequestRecord) -> Result<(), String> {
        self.put(REQUESTS, &record.id, record)
    }

    pub fn record_attempt(&self, record: &AttemptRecord) -> Result<(), String> {
        self.put(ATTEMPTS, &record.id, record)
    }

    pub fn record_alert(&self, record: &AlertRecord) -> Result<(), String> {
        self.put(ALERTS, &record.id, record)
    }

    pub fn requests(&self, limit: usize) -> Result<Vec<RequestRecord>, String> {
        self.list(REQUESTS, limit)
    }

    pub fn attempts(&self, limit: usize) -> Result<Vec<AttemptRecord>, String> {
        self.list(ATTEMPTS, limit)
    }

    pub fn alerts(&self, limit: usize) -> Result<Vec<AlertRecord>, String> {
        self.list(ALERTS, limit)
    }

    fn put<T: Serialize>(
        &self,
        definition: TableDefinition<&str, &[u8]>,
        key: &str,
        value: &T,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        {
            let mut table = transaction
                .open_table(definition)
                .map_err(|error| error.to_string())?;
            table
                .insert(key, bytes.as_slice())
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    fn list<T: DeserializeOwned>(
        &self,
        definition: TableDefinition<&str, &[u8]>,
        limit: usize,
    ) -> Result<Vec<T>, String> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let table = transaction
            .open_table(definition)
            .map_err(|error| error.to_string())?;
        let mut items = Vec::new();
        for entry in table.iter().map_err(|error| error.to_string())? {
            let (_, value) = entry.map_err(|error| error.to_string())?;
            let item = serde_json::from_slice(value.value()).map_err(|error| error.to_string())?;
            items.push(item);
        }
        if items.len() > limit {
            items.drain(0..items.len() - limit);
        }
        items.reverse();
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Example {
        value: u32,
    }

    #[test]
    fn round_trips_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.put_state("example", &Example { value: 7 }).unwrap();
        assert_eq!(
            store.get_state::<Example>("example").unwrap(),
            Some(Example { value: 7 })
        );
    }
}
