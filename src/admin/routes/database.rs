use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, Json};
use sqlx::PgPool;

use crate::state::AppState;

// Tables excluded from export/import — too large or contain transient data.
const EXCLUDED_TABLES: &[&str] = &["request_logs", "usage"];

async fn fetch_all_tables(db: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT IN ('sqlite_sequence') ORDER BY name"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
}

async fn fetch_columns(db: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        &format!("SELECT name FROM pragma_table_info('{}') ORDER BY cid", table)
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
}

#[derive(serde::Serialize)]
pub struct DatabaseInfo {
    pub url: String,
    pub size_bytes: i64,
    pub size_mb: f64,
    pub tables: Vec<TableInfo>,
    pub total_rows: i64,
    pub backup_count: i64,
}

#[derive(serde::Serialize)]
pub struct TableInfo {
    pub name: String,
    pub rows: i64,
    pub row_count: i64,
}

pub async fn api_database_info(State(state): State<Arc<AppState>>) -> Json<DatabaseInfo> {
    let cfg = &state.config;
    let url = cfg.database.url.clone();

    let size_bytes: i64 = if url.starts_with("sqlite:") {
        let stripped = url.trim_start_matches("sqlite:").split('?').next().unwrap_or(&url);
        std::path::Path::new(stripped)
            .metadata()
            .map(|m| m.len() as i64)
            .unwrap_or(0)
    } else { 0 };
    let size_mb = size_bytes as f64 / 1_048_576.0;

    let table_names = fetch_all_tables(&state.db).await;
    let mut tables = Vec::new();
    let mut total_rows: i64 = 0;

    for name in &table_names {
        let q = format!("SELECT COUNT(*) FROM \"{}\"", name);
        let rows: i64 = sqlx::query_scalar(&q)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
        total_rows += rows;
        tables.push(TableInfo { name: name.clone(), rows, row_count: rows });
    }

    let backup_count: i64 = std::path::Path::new("data/backups")
        .read_dir()
        .map(|d| d.flatten().count() as i64)
        .unwrap_or(0);

    Json(DatabaseInfo { url, size_bytes, size_mb, tables, total_rows, backup_count })
}

pub async fn api_database_export(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let table_names = fetch_all_tables(&state.db).await;
    let mut out: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    for name in &table_names {
        if EXCLUDED_TABLES.contains(&name.as_str()) { continue; }

        let cols = fetch_columns(&state.db, name).await;
        if cols.is_empty() { continue; }

        let q = format!("SELECT * FROM \"{}\"", name);
        let rows = match sqlx::query(&q).fetch_all(&state.db).await {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut table_data: Vec<serde_json::Value> = Vec::new();
        for row in &rows {
            let mut obj = serde_json::Map::new();
            for (i, col) in cols.iter().enumerate() {
                let val: serde_json::Value = match sqlx::Row::try_get::<String, _>(row, i) {
                    Ok(s) if s.starts_with('{') || s.starts_with('[') => {
                        serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                    }
                    Ok(s) => serde_json::Value::String(s),
                    Err(_) => {
                        match sqlx::Row::try_get::<i64, _>(row, i) { Ok(n) => {
                            serde_json::Value::Number(n.into())
                        } _ => { match sqlx::Row::try_get::<f64, _>(row, i) { Ok(f) => {
                            serde_json::Number::from_f64(f)
                                .map(|n| serde_json::Value::Number(n))
                                .unwrap_or(serde_json::Value::Null)
                        } _ => { match sqlx::Row::try_get::<bool, _>(row, i) { Ok(b) => {
                            serde_json::Value::Bool(b)
                        } _ => {
                            serde_json::Value::Null
                        }}}}}}
                    }
                };
                obj.insert(col.clone(), val);
            }
            table_data.push(serde_json::Value::Object(obj));
        }
        out.insert(name.clone(), table_data);
    }

    Json(serde_json::json!({
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "tables": out,
    }))
}

#[derive(serde::Deserialize)]
pub struct ImportRequest {
    pub tables: HashMap<String, Vec<serde_json::Value>>,
    pub replace: bool,
}

pub async fn api_database_import(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportRequest>,
) -> Json<serde_json::Value> {
    let allowed = fetch_all_tables(&state.db).await;
    let mut imported: i64 = 0;
    let mut errors: Vec<String> = Vec::new();

    for (table_name, rows) in req.tables.iter() {
        if !allowed.contains(table_name) {
            errors.push(format!("Table not allowed or doesn't exist: {}", table_name));
            continue;
        }
        if EXCLUDED_TABLES.contains(&table_name.as_str()) {
            errors.push(format!("Skipping excluded table: {}", table_name));
            continue;
        }
        if rows.is_empty() { continue; }

        // Get existing columns to match against
        let existing_cols = fetch_columns(&state.db, table_name).await;

        let first = match rows[0].as_object() {
            Some(o) => o,
            None => { errors.push(format!("Invalid row in {}", table_name)); continue; }
        };

        // Filter columns to only those that exist in the table
        let cols: Vec<String> = first.keys()
            .filter(|k| existing_cols.contains(k))
            .cloned()
            .collect();
        if cols.is_empty() { continue; }

        if req.replace {
            let del = format!("DELETE FROM \"{}\"", table_name);
            if let Err(e) = sqlx::query(&del).execute(&state.db).await {
                errors.push(format!("DELETE {} failed: {}", table_name, e));
                continue;
            }
        }

        let placeholders: String = (0..cols.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let col_list = cols.iter().map(|c| format!("\"{}\"", c)).collect::<Vec<_>>().join(",");
        let ins = format!("INSERT INTO \"{}\" ({}) VALUES ({})", table_name, col_list, placeholders);

        for row in rows {
            let obj = match row.as_object() { Some(o) => o, None => continue };
            let mut q = sqlx::query(&ins);
            for col in &cols {
                let v = obj.get(col).cloned().unwrap_or(serde_json::Value::Null);
                q = match v {
                    serde_json::Value::Null => q.bind(Option::<String>::None),
                    serde_json::Value::Bool(b) => q.bind(b as i64),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() { q.bind(i) }
                        else if let Some(f) = n.as_f64() { q.bind(f) }
                        else { q.bind(n.to_string()) }
                    }
                    serde_json::Value::String(s) => q.bind(s),
                    other => q.bind(other.to_string()),
                };
            }
            match q.execute(&state.db).await {
                Ok(r) => imported += r.rows_affected() as i64,
                Err(e) => errors.push(format!("Insert into {} failed: {}", table_name, e)),
            }
        }
    }

    Json(serde_json::json!({
        "ok": errors.is_empty(),
        "imported": imported,
        "errors": errors.len(),
        "error_details": errors,
    }))
}
