use std::{
    collections::BTreeMap,
    fs,
    ops::Bound::{Excluded, Unbounded},
    path::Path,
    sync::Arc,
};

use chrono::Utc;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::types::{AlertRecord, AttemptRecord, RequestRecord};

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("requests");
const ATTEMPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("attempts");
const ALERTS: TableDefinition<&str, &[u8]> = TableDefinition::new("alerts");
type RequestRollupValue = (u64, u64, u64, u64, u64, u64, u64);
type RequestBucketKey<'a> = (
    &'a str,
    u8,
    i64,
    u8,
    u32,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);
type RequestAllKey<'a> = (&'a str, u8, u32, &'a str, &'a str, &'a str, &'a str);
type AttemptRollupValue = (u64, u64, u64, u64, u64, u64, f64);

const REQUEST_ROLLUP_BUCKET: TableDefinition<RequestBucketKey<'static>, RequestRollupValue> =
    TableDefinition::new("request_rollup_bucket_v1");
const REQUEST_ROLLUP_ALL: TableDefinition<RequestAllKey<'static>, RequestRollupValue> =
    TableDefinition::new("request_rollup_all_v1");
const ATTEMPT_ROLLUP_ALL: TableDefinition<(&str, &str), AttemptRollupValue> =
    TableDefinition::new("attempt_rollup_all_v1");
const STATS_META: TableDefinition<&str, &[u8]> = TableDefinition::new("stats_meta_v1");

const STATS_META_KEY: &str = "rollup";
const STATS_SCHEMA_VERSION: u32 = 1;
const MIGRATION_BATCH_SIZE: usize = 1_000;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const ROLLUP_RETENTION_MS: i64 = 32 * 24 * HOUR_MS;
const SCOPE_MODEL: u8 = 0;
const SCOPE_TARGET: u8 = 1;
const SCOPE_UNATTRIBUTED: u8 = 2;
const GRAIN_MINUTE: u8 = 0;
const GRAIN_HOUR: u8 = 1;
const UNCLASSIFIED_MODEL: &str = "\0unclassified";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RequestRollup {
    pub calls: u64,
    pub errors: u64,
    pub fallbacks: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct AttemptRollup {
    pub attempts: u64,
    pub successes: u64,
    pub errors: u64,
    pub cache_reported_attempts: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RouteRollupKey {
    pub layer_index: u32,
    pub layer_name: String,
    pub provider: String,
    pub credential: String,
    pub upstream_model: String,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingRollup {
    pub total: RequestRollup,
    pub targets: BTreeMap<RouteRollupKey, RequestRollup>,
    pub unattributed: RequestRollup,
}

#[derive(Clone, Debug, Default)]
pub struct OverviewRollup {
    pub requests: RequestRollup,
    pub attempts: BTreeMap<(String, String), AttemptRollup>,
}

#[derive(Clone, Debug)]
pub struct RequestPage {
    pub requests: Vec<RequestRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatsPhase {
    Requests,
    Attempts,
    Ready,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StatsMeta {
    version: u32,
    phase: StatsPhase,
    request_cursor: Option<String>,
    attempt_cursor: Option<String>,
    migrated_requests: u64,
    migrated_attempts: u64,
}

impl Default for StatsMeta {
    fn default() -> Self {
        Self {
            version: STATS_SCHEMA_VERSION,
            phase: StatsPhase::Requests,
            request_cursor: None,
            attempt_cursor: None,
            migrated_requests: 0,
            migrated_attempts: 0,
        }
    }
}

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
        transaction
            .open_table(REQUEST_ROLLUP_BUCKET)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(REQUEST_ROLLUP_ALL)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(ATTEMPT_ROLLUP_ALL)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(STATS_META)
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(store)
    }

    pub fn ensure_stats_schema(&self) -> Result<(), String> {
        let mut meta = self.stats_meta()?.unwrap_or_default();
        if meta.version != STATS_SCHEMA_VERSION {
            return Err(format!(
                "unsupported statistics schema version {}; expected {STATS_SCHEMA_VERSION}",
                meta.version
            ));
        }
        loop {
            match meta.phase {
                StatsPhase::Requests => self.backfill_requests_batch(&mut meta)?,
                StatsPhase::Attempts => self.backfill_attempts_batch(&mut meta)?,
                StatsPhase::Ready => break,
            }
        }
        self.prune_expired_stats()
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
        let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        require_stats_ready(&transaction)?;
        {
            let mut requests = transaction
                .open_table(REQUESTS)
                .map_err(|error| error.to_string())?;
            if let Some(existing) = requests
                .get(record.id.as_str())
                .map_err(|error| error.to_string())?
            {
                if existing.value() == bytes.as_slice() {
                    return Ok(());
                }
                return Err(format!(
                    "request {} already exists with different contents",
                    record.id
                ));
            }
            requests
                .insert(record.id.as_str(), bytes.as_slice())
                .map_err(|error| error.to_string())?;
        }
        apply_request_rollup(&transaction, record, Utc::now().timestamp_millis())?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn record_attempt(&self, record: &AttemptRecord) -> Result<(), String> {
        let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        require_stats_ready(&transaction)?;
        {
            let mut attempts = transaction
                .open_table(ATTEMPTS)
                .map_err(|error| error.to_string())?;
            if let Some(existing) = attempts
                .get(record.id.as_str())
                .map_err(|error| error.to_string())?
            {
                if existing.value() == bytes.as_slice() {
                    return Ok(());
                }
                return Err(format!(
                    "attempt {} already exists with different contents",
                    record.id
                ));
            }
            attempts
                .insert(record.id.as_str(), bytes.as_slice())
                .map_err(|error| error.to_string())?;
        }
        apply_attempt_rollup(&transaction, record)?;
        transaction.commit().map_err(|error| error.to_string())
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

    pub fn request_page(&self, limit: usize, before: Option<&str>) -> Result<RequestPage, String> {
        let (requests, next_cursor) = self.list_page(REQUESTS, limit, before)?;
        Ok(RequestPage {
            requests,
            next_cursor,
        })
    }

    pub fn attempts(&self, limit: usize) -> Result<Vec<AttemptRecord>, String> {
        self.list(ATTEMPTS, limit)
    }

    pub fn alerts(&self, limit: usize) -> Result<Vec<AlertRecord>, String> {
        self.list(ALERTS, limit)
    }

    pub fn routing_rollup(
        &self,
        served_model: &str,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
    ) -> Result<RoutingRollup, String> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let mut result = RoutingRollup::default();
        match (from_ms, to_ms) {
            (None, None) => {
                let table = transaction
                    .open_table(REQUEST_ROLLUP_ALL)
                    .map_err(|error| error.to_string())?;
                let upper_model = format!("{served_model}\0");
                let start = (served_model, 0, 0, "", "", "", "");
                let end = (upper_model.as_str(), 0, 0, "", "", "", "");
                for entry in table.range(start..end).map_err(|error| error.to_string())? {
                    let (key, value) = entry.map_err(|error| error.to_string())?;
                    accumulate_request_row(&mut result, key.value(), value.value())?;
                }
            }
            (Some(from_ms), Some(to_ms)) if from_ms <= to_ms => {
                let table = transaction
                    .open_table(REQUEST_ROLLUP_BUCKET)
                    .map_err(|error| error.to_string())?;
                let first_full_hour = ceil_to(from_ms, HOUR_MS);
                let last_full_hour = floor_to(to_ms, HOUR_MS);
                let left_end = first_full_hour.min(to_ms);
                add_bucket_range(
                    &table,
                    served_model,
                    GRAIN_MINUTE,
                    from_ms,
                    left_end,
                    &mut result,
                )?;
                if first_full_hour < last_full_hour {
                    add_bucket_range(
                        &table,
                        served_model,
                        GRAIN_HOUR,
                        first_full_hour,
                        last_full_hour,
                        &mut result,
                    )?;
                }
                add_bucket_range(
                    &table,
                    served_model,
                    GRAIN_MINUTE,
                    last_full_hour.max(left_end),
                    to_ms,
                    &mut result,
                )?;
            }
            _ => return Err("statistics range must provide a valid from_ms and to_ms".into()),
        }
        Ok(result)
    }

    pub fn overview_rollup(&self) -> Result<OverviewRollup, String> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let mut result = OverviewRollup::default();
        {
            let table = transaction
                .open_table(REQUEST_ROLLUP_ALL)
                .map_err(|error| error.to_string())?;
            for entry in table.iter().map_err(|error| error.to_string())? {
                let (key, value) = entry.map_err(|error| error.to_string())?;
                if key.value().1 == SCOPE_MODEL {
                    result
                        .requests
                        .checked_add_assign(RequestRollup::from(value.value()))?;
                }
            }
        }
        {
            let table = transaction
                .open_table(ATTEMPT_ROLLUP_ALL)
                .map_err(|error| error.to_string())?;
            for entry in table.iter().map_err(|error| error.to_string())? {
                let (key, value) = entry.map_err(|error| error.to_string())?;
                let (provider, upstream_model) = key.value();
                result.attempts.insert(
                    (provider.to_string(), upstream_model.to_string()),
                    AttemptRollup::from(value.value()),
                );
            }
        }
        Ok(result)
    }

    pub fn prune_stats(&self, cutoff_ms: i64) -> Result<(), String> {
        let cutoff = floor_to(cutoff_ms, MINUTE_MS);
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        {
            let mut table = transaction
                .open_table(REQUEST_ROLLUP_BUCKET)
                .map_err(|error| error.to_string())?;
            table
                .retain(|key, _| key.2 >= cutoff)
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn prune_expired_stats(&self) -> Result<(), String> {
        self.prune_stats(
            Utc::now()
                .timestamp_millis()
                .saturating_sub(ROLLUP_RETENTION_MS),
        )
    }

    fn stats_meta(&self) -> Result<Option<StatsMeta>, String> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let table = transaction
            .open_table(STATS_META)
            .map_err(|error| error.to_string())?;
        let Some(value) = table
            .get(STATS_META_KEY)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        serde_json::from_slice(value.value()).map_err(|error| error.to_string())
    }

    fn backfill_requests_batch(&self, meta: &mut StatsMeta) -> Result<(), String> {
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        let rows = {
            let table = transaction
                .open_table(REQUESTS)
                .map_err(|error| error.to_string())?;
            read_batch::<RequestRecord>(&table, meta.request_cursor.as_deref())?
        };
        let now = Utc::now().timestamp_millis();
        for (_, record) in &rows {
            apply_request_rollup(&transaction, record, now)?;
        }
        if let Some((last_key, _)) = rows.last() {
            meta.request_cursor = Some(last_key.clone());
            meta.migrated_requests = meta
                .migrated_requests
                .checked_add(rows.len() as u64)
                .ok_or_else(|| "request migration counter overflow".to_string())?;
        }
        if rows.len() < MIGRATION_BATCH_SIZE {
            meta.phase = StatsPhase::Attempts;
        }
        write_stats_meta(&transaction, meta)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    fn backfill_attempts_batch(&self, meta: &mut StatsMeta) -> Result<(), String> {
        let transaction = self
            .database
            .begin_write()
            .map_err(|error| error.to_string())?;
        let rows = {
            let table = transaction
                .open_table(ATTEMPTS)
                .map_err(|error| error.to_string())?;
            read_batch::<AttemptRecord>(&table, meta.attempt_cursor.as_deref())?
        };
        for (_, record) in &rows {
            apply_attempt_rollup(&transaction, record)?;
        }
        if let Some((last_key, _)) = rows.last() {
            meta.attempt_cursor = Some(last_key.clone());
            meta.migrated_attempts = meta
                .migrated_attempts
                .checked_add(rows.len() as u64)
                .ok_or_else(|| "attempt migration counter overflow".to_string())?;
        }
        if rows.len() < MIGRATION_BATCH_SIZE {
            meta.phase = StatsPhase::Ready;
        }
        write_stats_meta(&transaction, meta)?;
        transaction.commit().map_err(|error| error.to_string())
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
        let mut items = Vec::with_capacity(limit);
        let entries = table.iter().map_err(|error| error.to_string())?;
        for entry in entries.rev().take(limit) {
            let (_, value) = entry.map_err(|error| error.to_string())?;
            let item = serde_json::from_slice(value.value()).map_err(|error| error.to_string())?;
            items.push(item);
        }
        Ok(items)
    }

    fn list_page<T: DeserializeOwned>(
        &self,
        definition: TableDefinition<&str, &[u8]>,
        limit: usize,
        before: Option<&str>,
    ) -> Result<(Vec<T>, Option<String>), String> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| error.to_string())?;
        let table = transaction
            .open_table(definition)
            .map_err(|error| error.to_string())?;
        let entries = if let Some(before) = before {
            table
                .range::<&str>((Unbounded, Excluded(before)))
                .map_err(|error| error.to_string())?
        } else {
            table.iter().map_err(|error| error.to_string())?
        };
        let mut items = Vec::with_capacity(limit);
        let mut last_key = None;
        let mut has_more = false;
        for (index, entry) in entries.rev().take(limit.saturating_add(1)).enumerate() {
            let (key, value) = entry.map_err(|error| error.to_string())?;
            if index == limit {
                has_more = true;
                break;
            }
            last_key = Some(key.value().to_string());
            items.push(serde_json::from_slice(value.value()).map_err(|error| error.to_string())?);
        }
        Ok((items, has_more.then_some(last_key).flatten()))
    }
}

impl From<RequestRollupValue> for RequestRollup {
    fn from(value: RequestRollupValue) -> Self {
        Self {
            calls: value.0,
            errors: value.1,
            fallbacks: value.2,
            input_tokens: value.3,
            output_tokens: value.4,
            total_tokens: value.5,
            bytes: value.6,
        }
    }
}

impl From<RequestRollup> for RequestRollupValue {
    fn from(value: RequestRollup) -> Self {
        (
            value.calls,
            value.errors,
            value.fallbacks,
            value.input_tokens,
            value.output_tokens,
            value.total_tokens,
            value.bytes,
        )
    }
}

impl RequestRollup {
    pub fn checked_add_assign(&mut self, other: Self) -> Result<(), String> {
        self.calls = checked_add(self.calls, other.calls, "calls")?;
        self.errors = checked_add(self.errors, other.errors, "errors")?;
        self.fallbacks = checked_add(self.fallbacks, other.fallbacks, "fallbacks")?;
        self.input_tokens = checked_add(self.input_tokens, other.input_tokens, "input tokens")?;
        self.output_tokens = checked_add(self.output_tokens, other.output_tokens, "output tokens")?;
        self.total_tokens = checked_add(self.total_tokens, other.total_tokens, "total tokens")?;
        self.bytes = checked_add(self.bytes, other.bytes, "bytes")?;
        Ok(())
    }
}

impl From<AttemptRollupValue> for AttemptRollup {
    fn from(value: AttemptRollupValue) -> Self {
        Self {
            attempts: value.0,
            successes: value.1,
            errors: value.2,
            cache_reported_attempts: value.3,
            cache_hit_tokens: value.4,
            cache_miss_tokens: value.5,
            cost_usd: value.6,
        }
    }
}

impl From<AttemptRollup> for AttemptRollupValue {
    fn from(value: AttemptRollup) -> Self {
        (
            value.attempts,
            value.successes,
            value.errors,
            value.cache_reported_attempts,
            value.cache_hit_tokens,
            value.cache_miss_tokens,
            value.cost_usd,
        )
    }
}

impl AttemptRollup {
    pub fn checked_add_assign(&mut self, other: Self) -> Result<(), String> {
        self.attempts = checked_add(self.attempts, other.attempts, "attempts")?;
        self.successes = checked_add(self.successes, other.successes, "attempt successes")?;
        self.errors = checked_add(self.errors, other.errors, "attempt errors")?;
        self.cache_reported_attempts = checked_add(
            self.cache_reported_attempts,
            other.cache_reported_attempts,
            "cache-reported attempts",
        )?;
        self.cache_hit_tokens = checked_add(
            self.cache_hit_tokens,
            other.cache_hit_tokens,
            "cache-hit tokens",
        )?;
        self.cache_miss_tokens = checked_add(
            self.cache_miss_tokens,
            other.cache_miss_tokens,
            "cache-miss tokens",
        )?;
        self.cost_usd += other.cost_usd;
        if !self.cost_usd.is_finite() {
            return Err("provider cost overflow".into());
        }
        Ok(())
    }
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, String> {
    left.checked_add(right)
        .ok_or_else(|| format!("statistics {field} overflow"))
}

fn read_batch<T: DeserializeOwned>(
    table: &redb::Table<'_, &str, &[u8]>,
    cursor: Option<&str>,
) -> Result<Vec<(String, T)>, String> {
    let mut rows = Vec::with_capacity(MIGRATION_BATCH_SIZE);
    if let Some(cursor) = cursor {
        let entries = table
            .range::<&str>((Excluded(cursor), Unbounded))
            .map_err(|error| error.to_string())?;
        for entry in entries.take(MIGRATION_BATCH_SIZE) {
            let (key, value) = entry.map_err(|error| error.to_string())?;
            rows.push((
                key.value().to_string(),
                serde_json::from_slice(value.value()).map_err(|error| error.to_string())?,
            ));
        }
    } else {
        let entries = table.iter().map_err(|error| error.to_string())?;
        for entry in entries.take(MIGRATION_BATCH_SIZE) {
            let (key, value) = entry.map_err(|error| error.to_string())?;
            rows.push((
                key.value().to_string(),
                serde_json::from_slice(value.value()).map_err(|error| error.to_string())?,
            ));
        }
    }
    Ok(rows)
}

fn write_stats_meta(transaction: &WriteTransaction, meta: &StatsMeta) -> Result<(), String> {
    let bytes = serde_json::to_vec(meta).map_err(|error| error.to_string())?;
    let mut table = transaction
        .open_table(STATS_META)
        .map_err(|error| error.to_string())?;
    table
        .insert(STATS_META_KEY, bytes.as_slice())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn require_stats_ready(transaction: &WriteTransaction) -> Result<(), String> {
    let table = transaction
        .open_table(STATS_META)
        .map_err(|error| error.to_string())?;
    let Some(value) = table
        .get(STATS_META_KEY)
        .map_err(|error| error.to_string())?
    else {
        return Err("statistics schema is not initialized".into());
    };
    let meta =
        serde_json::from_slice::<StatsMeta>(value.value()).map_err(|error| error.to_string())?;
    if meta.version != STATS_SCHEMA_VERSION || meta.phase != StatsPhase::Ready {
        return Err("statistics schema migration is not complete".into());
    }
    Ok(())
}

fn apply_request_rollup(
    transaction: &WriteTransaction,
    record: &RequestRecord,
    now_ms: i64,
) -> Result<(), String> {
    let served_model = record.served_model.as_deref().unwrap_or(UNCLASSIFIED_MODEL);
    let delta = RequestRollup {
        calls: 1,
        errors: u64::from(record.error_class.is_some()),
        fallbacks: u64::from(record.fallback),
        input_tokens: record.usage.input_tokens,
        output_tokens: record.usage.output_tokens,
        total_tokens: record.usage.total_tokens,
        bytes: record
            .request_bytes
            .checked_add(record.response_bytes)
            .ok_or_else(|| "request byte count overflow".to_string())?,
    };
    let target = request_target(record);
    let cutoff = now_ms.saturating_sub(ROLLUP_RETENTION_MS);
    if record.started_at_ms >= cutoff {
        let mut table = transaction
            .open_table(REQUEST_ROLLUP_BUCKET)
            .map_err(|error| error.to_string())?;
        for (grain, bucket_ms) in [
            (GRAIN_MINUTE, floor_to(record.started_at_ms, MINUTE_MS)),
            (GRAIN_HOUR, floor_to(record.started_at_ms, HOUR_MS)),
        ] {
            increment_request_bucket(
                &mut table,
                (
                    served_model,
                    grain,
                    bucket_ms,
                    SCOPE_MODEL,
                    0,
                    "",
                    "",
                    "",
                    "",
                ),
                delta,
            )?;
            match target.as_ref() {
                Some(target) => increment_request_bucket(
                    &mut table,
                    (
                        served_model,
                        grain,
                        bucket_ms,
                        SCOPE_TARGET,
                        target.layer_index,
                        target.layer_name.as_str(),
                        target.provider.as_str(),
                        target.credential.as_str(),
                        target.upstream_model.as_str(),
                    ),
                    delta,
                )?,
                None => increment_request_bucket(
                    &mut table,
                    (
                        served_model,
                        grain,
                        bucket_ms,
                        SCOPE_UNATTRIBUTED,
                        0,
                        "",
                        "",
                        "",
                        "",
                    ),
                    delta,
                )?,
            }
        }
    }
    let mut table = transaction
        .open_table(REQUEST_ROLLUP_ALL)
        .map_err(|error| error.to_string())?;
    increment_request_all(
        &mut table,
        (served_model, SCOPE_MODEL, 0, "", "", "", ""),
        delta,
    )?;
    match target {
        Some(target) => increment_request_all(
            &mut table,
            (
                served_model,
                SCOPE_TARGET,
                target.layer_index,
                target.layer_name.as_str(),
                target.provider.as_str(),
                target.credential.as_str(),
                target.upstream_model.as_str(),
            ),
            delta,
        ),
        None => increment_request_all(
            &mut table,
            (served_model, SCOPE_UNATTRIBUTED, 0, "", "", "", ""),
            delta,
        ),
    }
}

fn request_target(record: &RequestRecord) -> Option<RouteRollupKey> {
    Some(RouteRollupKey {
        layer_index: u32::try_from(record.route_layer_index?).ok()?,
        layer_name: record.route_layer.clone()?,
        provider: record.provider.clone()?,
        credential: record.credential.clone()?,
        upstream_model: record.upstream_model.clone()?,
    })
}

fn increment_request_bucket(
    table: &mut redb::Table<'_, RequestBucketKey<'static>, RequestRollupValue>,
    key: RequestBucketKey<'_>,
    delta: RequestRollup,
) -> Result<(), String> {
    let mut value = table
        .get(key)
        .map_err(|error| error.to_string())?
        .map(|value| RequestRollup::from(value.value()))
        .unwrap_or_default();
    value.checked_add_assign(delta)?;
    table
        .insert(key, RequestRollupValue::from(value))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn increment_request_all(
    table: &mut redb::Table<'_, RequestAllKey<'static>, RequestRollupValue>,
    key: RequestAllKey<'_>,
    delta: RequestRollup,
) -> Result<(), String> {
    let mut value = table
        .get(key)
        .map_err(|error| error.to_string())?
        .map(|value| RequestRollup::from(value.value()))
        .unwrap_or_default();
    value.checked_add_assign(delta)?;
    table
        .insert(key, RequestRollupValue::from(value))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn apply_attempt_rollup(
    transaction: &WriteTransaction,
    record: &AttemptRecord,
) -> Result<(), String> {
    let delta = AttemptRollup {
        attempts: 1,
        successes: u64::from(record.error_class.is_none()),
        errors: u64::from(record.error_class.is_some()),
        cache_reported_attempts: u64::from(record.usage.provider_reported),
        cache_hit_tokens: record.usage.cache_hit_tokens,
        cache_miss_tokens: record.usage.cache_miss_tokens,
        cost_usd: record.provider_cost_usd.unwrap_or(0.0),
    };
    if !delta.cost_usd.is_finite() {
        return Err("provider cost is not finite".into());
    }
    let key = (record.provider.as_str(), record.upstream_model.as_str());
    let mut table = transaction
        .open_table(ATTEMPT_ROLLUP_ALL)
        .map_err(|error| error.to_string())?;
    let mut value = table
        .get(key)
        .map_err(|error| error.to_string())?
        .map(|value| AttemptRollup::from(value.value()))
        .unwrap_or_default();
    value.checked_add_assign(delta)?;
    table
        .insert(key, AttemptRollupValue::from(value))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn add_bucket_range(
    table: &redb::ReadOnlyTable<RequestBucketKey<'static>, RequestRollupValue>,
    served_model: &str,
    grain: u8,
    start_ms: i64,
    end_ms: i64,
    result: &mut RoutingRollup,
) -> Result<(), String> {
    if start_ms >= end_ms {
        return Ok(());
    }
    let start = (served_model, grain, start_ms, 0, 0, "", "", "", "");
    let end = (served_model, grain, end_ms, 0, 0, "", "", "", "");
    for entry in table.range(start..end).map_err(|error| error.to_string())? {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        accumulate_request_bucket_row(result, key.value(), value.value())?;
    }
    Ok(())
}

fn accumulate_request_row(
    result: &mut RoutingRollup,
    key: RequestAllKey<'_>,
    value: RequestRollupValue,
) -> Result<(), String> {
    let (_, scope, layer_index, layer_name, provider, credential, upstream_model) = key;
    let value = RequestRollup::from(value);
    match scope {
        SCOPE_MODEL => result.total.checked_add_assign(value),
        SCOPE_TARGET => result
            .targets
            .entry(RouteRollupKey {
                layer_index,
                layer_name: layer_name.to_string(),
                provider: provider.to_string(),
                credential: credential.to_string(),
                upstream_model: upstream_model.to_string(),
            })
            .or_default()
            .checked_add_assign(value),
        SCOPE_UNATTRIBUTED => result.unattributed.checked_add_assign(value),
        _ => Err(format!("unknown request rollup scope {scope}")),
    }
}

fn accumulate_request_bucket_row(
    result: &mut RoutingRollup,
    key: RequestBucketKey<'_>,
    value: RequestRollupValue,
) -> Result<(), String> {
    let (model, _, _, scope, layer_index, layer_name, provider, credential, upstream_model) = key;
    accumulate_request_row(
        result,
        (
            model,
            scope,
            layer_index,
            layer_name,
            provider,
            credential,
            upstream_model,
        ),
        value,
    )
}

fn floor_to(value: i64, unit: i64) -> i64 {
    value.div_euclid(unit) * unit
}

fn ceil_to(value: i64, unit: i64) -> i64 {
    let floor = floor_to(value, unit);
    if floor == value {
        value
    } else {
        floor.saturating_add(unit)
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

    use crate::{
        config::ProviderKind,
        types::{Protocol, Usage},
    };

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

    #[test]
    fn records_and_queries_request_rollups_without_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.ensure_stats_schema().unwrap();
        let now = floor_to(Utc::now().timestamp_millis(), MINUTE_MS) - MINUTE_MS;
        let request = request_record("request-1", now);

        store.record_request(&request).unwrap();
        store.record_request(&request).unwrap();

        let all = store.routing_rollup("served", None, None).unwrap();
        assert_eq!(all.total.calls, 1);
        assert_eq!(all.total.input_tokens, 10);
        assert_eq!(all.total.output_tokens, 3);
        assert_eq!(all.targets.len(), 1);
        let (target, value) = all.targets.first_key_value().unwrap();
        assert_eq!(target.layer_index, 0);
        assert_eq!(target.layer_name, "plan");
        assert_eq!(target.provider, "provider");
        assert_eq!(target.credential, "key-a");
        assert_eq!(value.calls, 1);

        let window = store
            .routing_rollup("served", Some(now), Some(now + MINUTE_MS))
            .unwrap();
        assert_eq!(window.total, all.total);

        let mut changed = request.clone();
        changed.status = 500;
        assert!(store.record_request(&changed).is_err());
        assert_eq!(
            store
                .routing_rollup("served", None, None)
                .unwrap()
                .total
                .calls,
            1
        );
    }

    #[test]
    fn refuses_live_writes_until_the_stats_schema_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = Utc::now().timestamp_millis();
        assert!(
            store
                .record_request(&request_record("request-before-ready", now))
                .is_err()
        );
        assert!(
            store
                .record_attempt(&attempt_record("attempt-before-ready", now))
                .is_err()
        );
        assert!(store.requests(10).unwrap().is_empty());
        assert!(store.attempts(10).unwrap().is_empty());
    }

    #[test]
    fn unattributed_requests_reconcile_with_model_total() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.ensure_stats_schema().unwrap();
        let mut request = request_record("request-unattributed", Utc::now().timestamp_millis());
        request.provider = None;
        request.credential = None;
        request.route_layer = None;
        request.route_layer_index = None;
        request.upstream_model = None;
        store.record_request(&request).unwrap();

        let stats = store.routing_rollup("served", None, None).unwrap();
        assert_eq!(stats.total.calls, 1);
        assert_eq!(stats.unattributed.calls, 1);
        assert!(stats.targets.is_empty());
    }

    #[test]
    fn backfills_legacy_rows_once_and_builds_overview_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let request = request_record("legacy-request", Utc::now().timestamp_millis());
        let attempt = attempt_record("legacy-attempt", Utc::now().timestamp_millis());
        store.put(REQUESTS, &request.id, &request).unwrap();
        store.put(ATTEMPTS, &attempt.id, &attempt).unwrap();

        store.ensure_stats_schema().unwrap();
        store.ensure_stats_schema().unwrap();

        let overview = store.overview_rollup().unwrap();
        assert_eq!(overview.requests.calls, 1);
        let attempt_stats = overview
            .attempts
            .get(&("provider".into(), "upstream".into()))
            .unwrap();
        assert_eq!(attempt_stats.attempts, 1);
        assert_eq!(attempt_stats.successes, 1);
        assert_eq!(attempt_stats.cache_reported_attempts, 1);
        assert_eq!(attempt_stats.cache_hit_tokens, 4);
        assert_eq!(attempt_stats.cache_miss_tokens, 6);
        assert_eq!(attempt_stats.cost_usd, 0.25);
    }

    #[test]
    fn resumes_a_multi_batch_backfill_without_reapplying_completed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let transaction = store.database.begin_write().unwrap();
        {
            let mut table = transaction.open_table(REQUESTS).unwrap();
            for index in 0..=MIGRATION_BATCH_SIZE {
                let id = format!("legacy-{index:05}");
                let record = request_record(&id, Utc::now().timestamp_millis());
                let bytes = serde_json::to_vec(&record).unwrap();
                table.insert(id.as_str(), bytes.as_slice()).unwrap();
            }
        }
        transaction.commit().unwrap();

        let mut meta = StatsMeta::default();
        store.backfill_requests_batch(&mut meta).unwrap();
        assert_eq!(meta.phase, StatsPhase::Requests);
        assert_eq!(meta.migrated_requests, MIGRATION_BATCH_SIZE as u64);
        assert_eq!(
            store
                .routing_rollup("served", None, None)
                .unwrap()
                .total
                .calls,
            MIGRATION_BATCH_SIZE as u64
        );

        drop(store);
        let reopened = Store::open(dir.path()).unwrap();
        reopened.ensure_stats_schema().unwrap();
        reopened.ensure_stats_schema().unwrap();
        assert_eq!(
            reopened
                .routing_rollup("served", None, None)
                .unwrap()
                .total
                .calls,
            (MIGRATION_BATCH_SIZE + 1) as u64
        );
    }

    #[test]
    fn combines_partial_minute_ranges_with_full_hour_buckets_and_prunes_only_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.ensure_stats_schema().unwrap();
        let hour = floor_to(Utc::now().timestamp_millis(), HOUR_MS) - 4 * HOUR_MS;
        for (id, offset_minutes) in [("r0", 5), ("r1", 15), ("r2", 65), ("r3", 125)] {
            store
                .record_request(&request_record(id, hour + offset_minutes * MINUTE_MS))
                .unwrap();
        }

        let mixed = store
            .routing_rollup(
                "served",
                Some(hour + 10 * MINUTE_MS),
                Some(hour + 130 * MINUTE_MS),
            )
            .unwrap();
        assert_eq!(mixed.total.calls, 3);
        assert_eq!(mixed.total.total_tokens, 39);
        let exact_hour = store
            .routing_rollup("served", Some(hour + HOUR_MS), Some(hour + 2 * HOUR_MS))
            .unwrap();
        assert_eq!(exact_hour.total.calls, 1);

        store.prune_stats(hour + 2 * HOUR_MS).unwrap();
        let retained = store
            .routing_rollup("served", Some(hour), Some(hour + 3 * HOUR_MS))
            .unwrap();
        assert_eq!(retained.total.calls, 1);
        assert_eq!(
            store
                .routing_rollup("served", None, None)
                .unwrap()
                .total
                .calls,
            4
        );
    }

    #[test]
    fn attempt_rollups_are_idempotent_and_reject_conflicting_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.ensure_stats_schema().unwrap();
        let attempt = attempt_record("attempt-1", Utc::now().timestamp_millis());
        store.record_attempt(&attempt).unwrap();
        store.record_attempt(&attempt).unwrap();
        let stats = store.overview_rollup().unwrap();
        assert_eq!(stats.attempts.values().next().unwrap().attempts, 1);

        let mut changed = attempt;
        changed.provider_cost_usd = Some(0.5);
        assert!(store.record_attempt(&changed).is_err());
        assert_eq!(
            store
                .overview_rollup()
                .unwrap()
                .attempts
                .values()
                .next()
                .unwrap()
                .attempts,
            1
        );
    }

    #[test]
    fn recent_records_read_newest_keys_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for id in ["0001", "0002", "0003"] {
            let request = request_record(id, Utc::now().timestamp_millis());
            store.put(REQUESTS, id, &request).unwrap();
        }
        let rows = store.requests(2).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["0003", "0002"]
        );
    }

    #[test]
    fn request_pages_use_exclusive_bounded_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for id in ["0001", "0002", "0003", "0004", "0005"] {
            let request = request_record(id, Utc::now().timestamp_millis());
            store.put(REQUESTS, id, &request).unwrap();
        }

        let first = store.request_page(2, None).unwrap();
        assert_eq!(
            first
                .requests
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["0005", "0004"]
        );
        assert_eq!(first.next_cursor.as_deref(), Some("0004"));

        let newest = request_record("0006", Utc::now().timestamp_millis());
        store.put(REQUESTS, "0006", &newest).unwrap();
        let second = store.request_page(2, first.next_cursor.as_deref()).unwrap();
        assert_eq!(
            second
                .requests
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["0003", "0002"]
        );
        assert_eq!(second.next_cursor.as_deref(), Some("0002"));

        let last = store
            .request_page(2, second.next_cursor.as_deref())
            .unwrap();
        assert_eq!(
            last.requests
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["0001"]
        );
        assert_eq!(last.next_cursor, None);
    }

    fn request_record(id: &str, started_at_ms: i64) -> RequestRecord {
        RequestRecord {
            id: id.into(),
            started_at_ms,
            completed_at_ms: started_at_ms + 1,
            protocol: Protocol::OpenAiChat,
            requested_model: "served".into(),
            served_model: Some("served".into()),
            upstream_model: Some("upstream".into()),
            streaming: false,
            status: 200,
            error_class: None,
            provider: Some("provider".into()),
            provider_kind: Some(ProviderKind::DeepSeekOfficial),
            credential: Some("key-a".into()),
            route_layer: Some("plan".into()),
            route_layer_index: Some(0),
            selection_reason: Some("single-target".into()),
            matched_prefix_bytes: None,
            fallback: false,
            translated: false,
            request_bytes: 100,
            response_bytes: 200,
            first_byte_ms: Some(1),
            total_ms: 2,
            usage: Usage {
                input_tokens: 10,
                cache_hit_tokens: 4,
                cache_miss_tokens: 6,
                reasoning_tokens: 0,
                output_tokens: 3,
                total_tokens: 13,
                provider_reported: true,
            },
            claude_session_id: None,
            claude_agent_id: None,
            claude_parent_agent_id: None,
        }
    }

    fn attempt_record(id: &str, started_at_ms: i64) -> AttemptRecord {
        AttemptRecord {
            id: id.into(),
            request_id: Some("legacy-request".into()),
            sequence: 1,
            provider: "provider".into(),
            provider_kind: Some(ProviderKind::DeepSeekOfficial),
            credential: Some("key-a".into()),
            route_layer: Some("plan".into()),
            route_layer_index: Some(0),
            selection_reason: Some("single-target".into()),
            matched_prefix_bytes: None,
            upstream_model: "upstream".into(),
            egress_protocol: Protocol::OpenAiChat,
            translated: false,
            probe: false,
            started_at_ms,
            completed_at_ms: started_at_ms + 1,
            status: Some(200),
            error_class: None,
            retry_after_ms: None,
            committed: true,
            request_bytes: 100,
            response_bytes: 200,
            first_byte_ms: Some(1),
            total_ms: 2,
            usage: request_record("usage", started_at_ms).usage,
            provider_cost_usd: Some(0.25),
            sanitized_error: None,
        }
    }
}
