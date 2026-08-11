use std::{fs, path::Path, sync::Arc};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

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

    pub fn rename_provider(&self, old: &str, new: &str) -> Result<usize, String> {
        if old == new {
            return Ok(0);
        }
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        let mut changed = 0;
        changed += rewrite_provider_field(&transaction, REQUESTS, old, new)?;
        changed += rewrite_provider_field(&transaction, ATTEMPTS, old, new)?;
        changed += rewrite_provider_field(&transaction, ALERTS, old, new)?;
        changed += rename_circuit_state_keys(&transaction, old, new)?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(changed)
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

fn rewrite_provider_field(
    transaction: &WriteTransaction,
    definition: TableDefinition<&str, &[u8]>,
    old: &str,
    new: &str,
) -> Result<usize, String> {
    let mut table = transaction
        .open_table(definition)
        .map_err(|error| error.to_string())?;
    let mut updates = Vec::new();
    {
        let entries = table.iter().map_err(|error| error.to_string())?;
        for entry in entries {
            let (key, value) = entry.map_err(|error| error.to_string())?;
            let mut record: Value =
                serde_json::from_slice(value.value()).map_err(|error| error.to_string())?;
            if record.get("provider").and_then(Value::as_str) != Some(old) {
                continue;
            }
            record["provider"] = Value::String(new.to_string());
            updates.push((
                key.value().to_string(),
                serde_json::to_vec(&record).map_err(|error| error.to_string())?,
            ));
        }
    }
    for (key, value) in &updates {
        table
            .insert(key.as_str(), value.as_slice())
            .map_err(|error| error.to_string())?;
    }
    Ok(updates.len())
}

fn rename_circuit_state_keys(
    transaction: &WriteTransaction,
    old: &str,
    new: &str,
) -> Result<usize, String> {
    let mut table = transaction
        .open_table(STATE)
        .map_err(|error| error.to_string())?;
    let old_prefix = format!("circuit:{old}:");
    let new_prefix = format!("circuit:{new}:");
    let mut moves = Vec::new();
    {
        let entries = table.iter().map_err(|error| error.to_string())?;
        for entry in entries {
            let (key, value) = entry.map_err(|error| error.to_string())?;
            let key = key.value();
            let Some(suffix) = key.strip_prefix(&old_prefix) else {
                continue;
            };
            moves.push((
                key.to_string(),
                format!("{new_prefix}{suffix}"),
                value.value().to_vec(),
            ));
        }
    }
    for (old_key, new_key, value) in &moves {
        let destination_exists = table
            .get(new_key.as_str())
            .map_err(|error| error.to_string())?
            .is_some();
        if !destination_exists {
            table
                .insert(new_key.as_str(), value.as_slice())
                .map_err(|error| error.to_string())?;
        }
        table
            .remove(old_key.as_str())
            .map_err(|error| error.to_string())?;
    }
    Ok(moves.len())
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

    #[test]
    fn renames_provider_records_and_circuit_keys_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .put(
                REQUESTS,
                "request",
                &serde_json::json!({"provider":"open-code-go"}),
            )
            .unwrap();
        store
            .put(
                ATTEMPTS,
                "attempt",
                &serde_json::json!({"provider":"open-code-go"}),
            )
            .unwrap();
        store
            .put(
                ALERTS,
                "alert",
                &serde_json::json!({"provider":"open-code-go"}),
            )
            .unwrap();
        store
            .put_state(
                "circuit:open-code-go:key:model",
                &serde_json::json!({"mode":"closed"}),
            )
            .unwrap();

        assert_eq!(
            store
                .rename_provider("open-code-go", "opencode-go")
                .unwrap(),
            4
        );
        assert_eq!(
            store
                .rename_provider("open-code-go", "opencode-go")
                .unwrap(),
            0
        );

        for (definition, key) in [
            (REQUESTS, "request"),
            (ATTEMPTS, "attempt"),
            (ALERTS, "alert"),
        ] {
            let transaction = store.database.begin_read().unwrap();
            let table = transaction.open_table(definition).unwrap();
            let record: Value =
                serde_json::from_slice(table.get(key).unwrap().unwrap().value()).unwrap();
            assert_eq!(record["provider"], "opencode-go");
        }
        assert!(
            store
                .get_state::<Value>("circuit:open-code-go:key:model")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .get_state::<Value>("circuit:opencode-go:key:model")
                .unwrap()
                .unwrap()["mode"],
            "closed"
        );
    }
}
