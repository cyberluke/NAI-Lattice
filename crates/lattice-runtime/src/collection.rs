//! Lattice collection projection + transaction + lease surfaces (`collection.v1`,
//! `transaction.v1`, `lease.v1`).
//!
//! Powers Mastra canonical persistence (`LatticeMastraStore`/`LatticeFactoryStorage`)
//! on NAI-Lattice instead of a LibSQL/SQLite island. The **ledger is the durable
//! source of truth**; this module is a rebuildable in-memory projection of the
//! `nai.mastra.collection.<name>.mutation` ledger events.
//!
//! Concurrency: the runtime guards this store with a single `tokio::sync::Mutex`.
//! That one serialized region is exactly LibSQL's single-writer path — the same
//! isolation `FactoryStorage.updateAtomic` documents. `update_atomic` and
//! `withTransaction` hold the lock across their RMW, so a concurrent mutation can
//! never interleave and be lost. Reads that precede a pending durable ack may see
//! a stale projection (the ledger is authoritative); this is eventual consistency
//! of a cache, never a durability downgrade.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use uuid::Uuid;

/// Column type, mirrored 1:1 from the Mastra `FactoryStorageOps` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Text,
    BigInt,
    Integer,
    Boolean,
    Json,
    Timestamp,
    UuidPk,
}

impl ColumnType {
    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "text" => Self::Text,
            "bigint" => Self::BigInt,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "json" => Self::Json,
            "timestamp" => Self::Timestamp,
            "uuid-pk" => Self::UuidPk,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub kind: ColumnType,
    pub nullable: bool,
    pub primary_key: bool,
    pub default: Option<Value>,
}

/// Declared collection schema. Additive evolution only (matches Mastra).
#[derive(Debug, Clone)]
pub struct CollectionSchemaDef {
    pub name: String,
    pub columns: Vec<(String, ColumnSpec)>,
    pub primary_key_columns: Vec<String>,
    /// `(name, columns)` plain indexes.
    #[allow(dead_code)]
    pub indexes: Vec<(String, Vec<String>)>,
    /// `(name, columns, partial)` unique indexes.
    #[allow(dead_code)]
    pub unique_indexes: Vec<(String, Vec<String>, Option<String>)>,
}

impl CollectionSchemaDef {
    /// Parse the Mastra `CollectionSchema` JSON shape.
    pub fn from_json(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let cols = v.get("columns")?.as_object()?;
        let mut columns = Vec::with_capacity(cols.len());
        let mut primary_key_columns = Vec::new();
        for (col_name, spec) in cols {
            let t = spec.get("type").and_then(|x| x.as_str())?;
            let kind = ColumnType::from_str(t)?;
            let pk = spec.get("primaryKey").and_then(|x| x.as_bool()).unwrap_or(false);
            let mut pk_cols_all = spec.get("columns").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect::<Vec<_>>())
                .unwrap_or_default();
            if pk && pk_cols_all.is_empty() {
                pk_cols_all.push(col_name.clone());
            }
            primary_key_columns.extend(pk_cols_all);
            columns.push((
                col_name.clone(),
                ColumnSpec {
                    kind,
                    nullable: spec.get("nullable").and_then(|x| x.as_bool()).unwrap_or(false),
                    primary_key: pk,
                    default: spec.get("default").cloned(),
                },
            ));
        }
        let mut indexes = Vec::new();
        if let Some(arr) = v.get("indexes").and_then(|i| i.as_array()) {
            for idx in arr {
                let iname = idx.get("name")?.as_str()?.to_string();
                let mut cols = Vec::new();
                for c in idx.get("columns")?.as_array()? {
                    cols.push(c.as_str()?.to_string());
                }
                indexes.push((iname, cols));
            }
        }
        let mut unique_indexes = Vec::new();
        if let Some(arr) = v.get("uniqueIndexes").and_then(|i| i.as_array()) {
            for u in arr {
                let uname = u.get("name")?.as_str()?.to_string();
                let mut cols = Vec::new();
                for c in u.get("columns")?.as_array()? {
                    cols.push(c.as_str()?.to_string());
                }
                let partial = u.get("whereNotNull").and_then(|x| x.as_str())
                    .or_else(|| u.get("whereNull").and_then(|x| x.as_str()))
                    .map(str::to_string);
                unique_indexes.push((uname, cols, partial));
            }
        }
        Some(Self {
            name,
            columns,
            primary_key_columns,
            indexes,
            unique_indexes,
        })
    }

    /// Canonical row id: the serialized primary key columns, or `id` when the
    /// schema declared a `uuid-pk`/natural pk without explicit composite.
    pub fn row_id(&self, row: &Value) -> String {
        if let Some(id) = row.get("id").and_then(|x| x.as_str()) {
            return id.to_string();
        }
        if !self.primary_key_columns.is_empty() {
            let mut out = String::new();
            for pk in &self.primary_key_columns {
                out.push_str(&row.get(pk).map(|v| v.to_string()).unwrap_or_else(|| "null".into()));
                out.push('|');
            }
            return out;
        }
        format!("row-{}", Uuid::now_v7())
    }
}

/// A pending transaction session. Holds staged, uncommitted mutation deltas as
/// `(collection, delta)` pairs so the runtime can atomically batch them.
pub struct PendingTx {
    /// Staged `(collection, canonical_delta)` pairs, applied at commit.
    pub deltas: Vec<(String, Value)>,
}

/// An active lease (Mastra workflow run ownership / fencing).
#[derive(Debug, Clone)]
pub struct LeaseRecord {
    pub owner_id: String,
    pub generation: u64,
    pub expires_at_ms: i64,
}

/// The collection projection store (guarded by a single runtime `Mutex`).
pub struct CollectionStore {
    schemas: HashMap<String, CollectionSchemaDef>,
    /// collection name -> row id -> current row.
    rows: HashMap<String, BTreeMap<String, Value>>,
    /// collection name -> bump counter, increment on every committed mutation.
    generations: HashMap<String, u64>,
    /// tx_id -> pending transaction.
    transactions: HashMap<String, PendingTx>,
    /// lease_id -> lease record.
    leases: HashMap<String, LeaseRecord>,
}

impl CollectionStore {
    pub fn new() -> Self {
        Self {
            schemas: HashMap::new(),
            rows: HashMap::new(),
            generations: HashMap::new(),
            transactions: HashMap::new(),
            leases: HashMap::new(),
        }
    }

    fn schema(&self, name: &str) -> std::result::Result<&CollectionSchemaDef, String> {
        self.schemas
            .get(name)
            .ok_or_else(|| format!("collection {name} not registered; call collection.schema.ensure first"))
    }

    /// Register (or additively evolve) a collection schema.
    pub fn ensure_schema(&mut self, v: &Value) -> Result<Value, String> {
        let def = CollectionSchemaDef::from_json(v).ok_or("invalid collection schema JSON")?;
        let name = def.name.clone();
        if self.schemas.contains_key(&name) {
            // Additive: keep the existing schema. A caller re-ensuring the same
            // repo must be a no-op so it is idempotent across restarts.
            return Ok(serde_json::json!({ "collection": name, "existing": true }));
        }
        self.schemas.insert(name.clone(), def);
        Ok(serde_json::json!({ "collection": name, "created": true }))
    }

    /// Equality / IN / IS NULL filter. `{}` matches every row.
    pub fn matches(&self, row: &Value, where_json: &Value) -> bool {
        match where_json.as_object() {
            None => true,
            Some(cond) => {
                if cond.is_empty() {
                    return true;
                }
                cond.iter().all(|(col, expected)| {
                    let cell = row.get(col);
                    if let Some(in_list) = expected.get("in").and_then(|x| x.as_array()) {
                        return in_list.iter().any(|e| cell == Some(e));
                    }
                    if expected.is_null() {
                        return cell.is_none() || cell == Some(&Value::Null);
                    }
                    cell == Some(expected)
                })
            }
        }
    }

    /// Locate a row matching `where_json` (first match, stable order).
    pub fn find_one(&self, collection: &str, where_json: &Value) -> Result<Option<Value>, String> {
        let _ = self.schema(collection)?;
        let mut out = None;
        if let Some(table) = self.rows.get(collection) {
            for row in table.values() {
                if self.matches(row, where_json) {
                    out = Some(row.clone());
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Ordered list query with optional limit (Mastra keyset cursor is handled
    /// by the runtime; here we provide the filtered, optionally-ordered set).
    pub fn find_many(
        &self,
        collection: &str,
        where_json: &Value,
        order_by: &Value,
        limit: Option<usize>,
    ) -> Result<Vec<Value>, String> {
        let _ = self.schema(collection)?;
        let mut out: Vec<Value> = Vec::new();
        if let Some(table) = self.rows.get(collection) {
            for row in table.values() {
                if self.matches(row, where_json) {
                    out.push(row.clone());
                }
            }
        }
        if let Some(ob) = order_by.as_array() {
            let cols: Vec<(String, bool)> = ob
                .iter()
                .filter_map(|c| {
                    let arr = c.as_array()?;
                    let col = arr.first()?.as_str()?.to_string();
                    let desc = arr.get(1).and_then(|d| d.as_str()).unwrap_or("asc") == "desc";
                    Some((col, desc))
                })
                .collect();
            if !cols.is_empty() {
                out.sort_by(|a, b| {
                    for (col, desc) in &cols {
                        let cmp = cmp_json(a.get(col), b.get(col));
                        let cmp = if *desc { cmp.reverse() } else { cmp };
                        if cmp != std::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    cmp_json(a.get("id"), b.get("id"))
                });
            }
        }
        if let Some(l) = limit {
            out.truncate(l);
        }
        Ok(out)
    }

    pub fn count(&self, collection: &str, where_json: &Value) -> Result<u64, String> {
        let _ = self.schema(collection)?;
        let n = match self.rows.get(collection) {
            Some(table) => table.values().filter(|r| self.matches(r, where_json)).count() as u64,
            None => 0,
        };
        Ok(n)
    }

    /// Build the canonical mutation delta for an upsert/insert. When `conflict_keys`
    /// is non-empty and matches an existing row, this becomes an in-place update of
    /// that row; otherwise it is an insert (generating an id if the schema uses a
    /// `uuid-pk` and the row has none). The returned `Value` is the `nai.mastra.
    /// collection.<name>.mutation` ledger payload the runtime must append (Durable)
    /// and then feed to `apply_delta` once the durable ACK arrives.
    pub fn plan_upsert(
        &mut self,
        collection: &str,
        row: &Value,
        conflict_keys: &[String],
    ) -> Result<Value, String> {
        let schema = self.schema(collection)?.clone();
        let mut next = row.clone();
        // Generate a `uuid-pk` id if the schema wants one and the row lacks it.
        if !schema.primary_key_columns.is_empty()
            && next.get("id").is_none()
            && schema.primary_key_columns.iter().any(|pk| pk == "id")
        {
            if let Some(obj) = next.as_object_mut() {
                obj.insert("id".into(), Value::String(Uuid::now_v7().to_string()));
            }
        }
        if !conflict_keys.is_empty() {
            if let Some(table) = self.rows.get(collection) {
                if let Some(existing) = table.values().find(|existing| {
                    conflict_keys.iter().all(|k| existing.get(k) == next.get(k))
                }) {
                    // Merge new columns into the existing row (preserve PK).
                    let rid = schema.row_id(existing);
                    let mut merged = existing.clone();
                    if let Some(obj) = next.as_object() {
                        for (k, v) in obj {
                            merged[k.clone()] = v.clone();
                        }
                    }
                    return Ok(serde_json::json!({
                        "kind": "upsert",
                        "row_id": rid,
                        "row": merged,
                    }));
                }
            }
        }
        let rid = schema.row_id(&next);
        Ok(serde_json::json!({ "kind": "upsert", "row_id": rid, "row": next }))
    }

    /// Plan `updateMany` / `deleteMany` as canonical deltas carrying the set of
    /// affected row ids (computed against the current projection).
    pub fn plan_update_many(
        &mut self,
        collection: &str,
        where_json: &Value,
        set: Option<&Value>,
    ) -> Result<Value, String> {
        let _ = self.schema(collection)?;
        let mut ids = Vec::new();
        let mut updated: Vec<Value> = Vec::new();
        if let Some(table) = self.rows.get(collection) {
            for (rid, row) in table {
                if self.matches(row, where_json) {
                    ids.push(rid.clone());
                    if let Some(s) = set {
                        let mut next = row.clone();
                        if let Some(obj) = s.as_object() {
                            for (k, v) in obj {
                                next[k.clone()] = v.clone();
                            }
                        }
                        updated.push(next);
                    }
                }
            }
        }
        Ok(serde_json::json!({
            "kind": if set.is_some() { "update" } else { "delete" },
            "row_ids": ids,
            "rows": updated,
        }))
    }

    /// Apply a committed canonical delta to the projection and bump the
    /// collection generation. Only call after the durable ledger ACK.
    pub fn apply_delta(&mut self, collection: &str, delta: &Value) -> Result<(), String> {
        let schema = self.schema(collection)?.clone();
        let kind = delta.get("kind").and_then(|k| k.as_str()).unwrap_or("upsert");
        let table = self.rows.entry(collection.to_string()).or_default();
        match kind {
            "upsert" => {
                let row = delta.get("row").cloned().unwrap_or(Value::Null);
                // Prefer the row id the planner computed; fall back to deriving it.
                let rid = delta
                    .get("row_id")
                    .and_then(|r| r.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| schema.row_id(&row));
                table.insert(rid, row);
            }
            "update" | "delete" => {
                if let Some(ids) = delta.get("row_ids").and_then(|x| x.as_array()) {
                    let rows: Vec<Value> = delta
                        .get("rows")
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for (i, id) in ids.iter().enumerate() {
                        if let Some(s) = id.as_str() {
                            if kind == "delete" {
                                table.remove(s);
                            } else if let Some(new_row) = rows.get(i) {
                                // Aligned by construction: row_ids[i] <-> rows[i].
                                table.insert(s.to_string(), new_row.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let g = self.generations.entry(collection.to_string()).or_insert(0);
        *g += 1;
        Ok(())
    }

    /// Current generation of a collection (for serializable conflict detection).
    pub fn generation(&self, collection: &str) -> u64 {
        self.generations.get(collection).copied().unwrap_or(0)
    }

    // ---- transactions ----

    pub fn begin(&mut self) -> String {
        let tx_id = Uuid::now_v7().to_string();
        self.transactions.insert(tx_id.clone(), PendingTx { deltas: Vec::new() });
        tx_id
    }

    pub fn stage(&mut self, tx_id: &str, collection: &str, delta: Value) -> Result<(), String> {
        let tx = self
            .transactions
            .get_mut(tx_id)
            .ok_or_else(|| format!("transaction {tx_id} not found"))?;
        tx.deltas.push((collection.to_string(), delta));
        Ok(())
    }

    /// Take the staged deltas, atomically applied at commit. Consumes the tx.
    pub fn commit_plan(&mut self, tx_id: &str) -> Result<Vec<(String, Value)>, String> {
        let tx = self
            .transactions
            .remove(tx_id)
            .ok_or_else(|| format!("transaction {tx_id} not found or already committed"))?;
        Ok(tx.deltas)
    }

    pub fn rollback(&mut self, tx_id: &str) -> Result<(), String> {
        self.transactions
            .remove(tx_id)
            .map(|_| ())
            .ok_or_else(|| format!("transaction {tx_id} not found"))
    }

    // ---- leases ----

    /// Claim a lease; returns the {generation, expires_at} or a conflict error if
    /// an unexpired lease with a different owner already holds it.
    pub fn lease_claim(
        &mut self,
        lease_id: &str,
        owner_id: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<Value, String> {
        let existing = self.leases.get(lease_id);
        if let Some(l) = existing {
            if l.owner_id != owner_id && l.expires_at_ms > now_ms {
                return Err(format!("lease {lease_id} held by {0}", l.owner_id));
            }
        }
        let gen = existing.map(|l| l.generation + 1).unwrap_or(1);
        self.leases.insert(
            lease_id.to_string(),
            LeaseRecord {
                owner_id: owner_id.to_string(),
                generation: gen,
                expires_at_ms: now_ms + ttl_ms,
            },
        );
        Ok(serde_json::json!({ "generation": gen, "expires_at_ms": now_ms + ttl_ms }))
    }

    pub fn lease_renew(
        &mut self,
        lease_id: &str,
        owner_id: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<Value, String> {
        let mut l = self
            .leases
            .get(lease_id)
            .cloned()
            .ok_or_else(|| format!("lease {lease_id} not found"))?;
        if l.owner_id != owner_id {
            return Err(format!("lease {lease_id} owned by {}, not {owner_id}", l.owner_id));
        }
        l.expires_at_ms = now_ms + ttl_ms;
        self.leases.insert(lease_id.to_string(), l.clone());
        Ok(serde_json::json!({ "generation": l.generation, "expires_at_ms": l.expires_at_ms }))
    }

    pub fn lease_release(
        &mut self,
        lease_id: &str,
        owner_id: &str,
    ) -> Result<Value, String> {
        let removed = self.leases.remove(lease_id);
        match removed {
            Some(l) if l.owner_id == owner_id => Ok(serde_json::json!({ "released": true })),
            Some(l) => Err(format!(
                "lease {lease_id} owned by {}, not {owner_id}",
                l.owner_id
            )),
            None => Err(format!("lease {lease_id} not found")),
        }
    }
}

fn cmp_json(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => match (x.as_str(), y.as_str()) {
            (Some(xs), Some(ys)) => xs.cmp(ys),
            _ => match (x.as_f64(), y.as_f64()) {
                (Some(xn), Some(yn)) => xn.partial_cmp(&yn).unwrap_or(std::cmp::Ordering::Equal),
                _ => x.to_string().cmp(&y.to_string()),
            },
        },
    }
}