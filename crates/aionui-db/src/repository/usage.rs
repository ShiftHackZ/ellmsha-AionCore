//! Durable per-request counters. No prompts, tool arguments or credentials are stored.
use crate::DbError;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

fn reported_by_default() -> bool {
    true
}

/// Provider session totals, never context occupancy. Persisted high-water marks
/// make reconnect/replayed notifications safe. An unseen resumed session is
/// baselined rather than charging historical tokens to today's usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageCounter {
    pub source: String,
    #[serde(default)]
    pub baseline_if_unknown: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub request_id: String,
    #[serde(default)]
    pub counter: Option<UsageCounter>,
    #[serde(default = "reported_by_default")]
    pub billing_reported: bool,
    #[serde(default = "reported_by_default")]
    pub context_reported: bool,
    pub model: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub is_child: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub context_tokens: u64,
    pub context_window: u64,
    pub breakdown: serde_json::Value,
    /// Content-free pre-response request sizes; absent for opaque CLI/ACP adapters.
    #[serde(default)]
    pub request_metrics: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageQuery {
    /// Rolling duration in days, 1..365; global default is 30 days.
    pub days: Option<u16>,
    /// Inclusive Unix milliseconds. A conversation without explicit range uses all time.
    pub start: Option<i64>,
    /// Exclusive Unix milliseconds; defaults to now.
    pub end: Option<i64>,
    pub conversation_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl UsageQuery {
    /// Validate range before arithmetic; malformed authenticated queries must not panic.
    pub fn period(&self) -> Result<(i64, i64), &'static str> {
        let days = self.days.unwrap_or(30);
        if days == 0 || days > 365 {
            return Err("Usage days must be between 1 and 365");
        }
        let end = self.end.unwrap_or_else(|| aionui_common::now_ms().saturating_add(1));
        let all_time_chat = self.conversation_id.as_ref().is_some_and(|id| !id.is_empty())
            && self.days.is_none()
            && self.start.is_none()
            && self.end.is_none();
        let start = match self.start {
            None if all_time_chat => 0,
            Some(start) => start,
            None => end
                .checked_sub(i64::from(days) * 86_400_000)
                .ok_or("Invalid usage period")?,
        };
        if start < 0 || end < start || (!all_time_chat && end - start > 366 * 86_400_000_i64) {
            return Err("Usage period must be between 0 and 366 days");
        }
        Ok((start, end))
    }
}

pub struct SqliteUsageRepository {
    pool: SqlitePool,
}
impl SqliteUsageRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, user: &str, conversation: &str, report: &UsageReport) -> Result<(), DbError> {
        if report.request_id.is_empty()
            || [
                report.input_tokens,
                report.output_tokens,
                report.cache_read_tokens,
                report.cache_creation_tokens,
                report.context_tokens,
                report.context_window,
            ]
            .iter()
            .any(|v| *v > i64::MAX as u64)
        {
            return Err(DbError::Conflict("Invalid usage counters".into()));
        }
        // Acquire the writer reservation before reading the cursor/ownership.
        // A deferred read transaction cannot upgrade a stale WAL snapshot when
        // message persistence wins the race (SQLITE_BUSY_SNAPSHOT).
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id=? AND user_id=?")
            .bind(conversation)
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
        if owned == 0 {
            return Ok(());
        }
        let mut measured = report.clone();
        if let Some(counter) = &report.counter {
            if counter.source.is_empty() || counter.source.len() > 1024 {
                return Err(DbError::Conflict("Invalid usage counter source".into()));
            }
            let previous = sqlx::query("SELECT input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens FROM usage_cursors WHERE user_id=? AND conversation_id=? AND source=?")
                .bind(user).bind(conversation).bind(&counter.source).fetch_optional(&mut *tx).await?;
            let current = [
                report.input_tokens,
                report.output_tokens,
                report.cache_read_tokens,
                report.cache_creation_tokens,
            ];
            let old = previous.as_ref().map(|r| {
                [
                    r.get::<i64, _>(0) as u64,
                    r.get::<i64, _>(1) as u64,
                    r.get::<i64, _>(2) as u64,
                    r.get::<i64, _>(3) as u64,
                ]
            });
            let base = old.unwrap_or(if counter.baseline_if_unknown { current } else { [0; 4] });
            sqlx::query("INSERT INTO usage_cursors VALUES(?,?,?,?,?,?,?) ON CONFLICT(user_id,conversation_id,source) DO UPDATE SET input_tokens=MAX(input_tokens,excluded.input_tokens),output_tokens=MAX(output_tokens,excluded.output_tokens),cache_read_tokens=MAX(cache_read_tokens,excluded.cache_read_tokens),cache_creation_tokens=MAX(cache_creation_tokens,excluded.cache_creation_tokens)")
                .bind(user).bind(conversation).bind(&counter.source).bind(current[0] as i64).bind(current[1] as i64).bind(current[2] as i64).bind(current[3] as i64).execute(&mut *tx).await?;
            measured.input_tokens = current[0].saturating_sub(base[0]);
            measured.output_tokens = current[1].saturating_sub(base[1]);
            measured.cache_read_tokens = current[2].saturating_sub(base[2]);
            measured.cache_creation_tokens = current[3].saturating_sub(base[3]);
            if measured.input_tokens == 0 && measured.output_tokens == 0 {
                if old.is_none() && report.context_tokens > 0 {
                    measured.billing_reported = false;
                } else {
                    tx.commit().await?;
                    return Ok(());
                }
            }
        }
        let report = &measured;
        // Resolve provider from the owned conversation, never from model-authored data.
        // Request IDs are generated by the engine and deduplicate repeated delivery.
        sqlx::query("INSERT OR IGNORE INTO usage_requests SELECT ?,c.user_id,c.id,COALESCE(?,json_extract(c.model,'$.provider_id'),(SELECT s.agent_id FROM acp_session s WHERE s.conversation_id=c.id),c.type),?,?,?,?,?,?,?,?,?,?,?,?,? FROM conversations c WHERE c.id=? AND c.user_id=?")
            .bind(&report.request_id).bind(&report.provider_id).bind(&report.model).bind(report.is_child)
            .bind(report.input_tokens as i64).bind(report.output_tokens as i64)
            .bind(report.cache_read_tokens as i64).bind(report.cache_creation_tokens as i64)
            .bind(report.context_tokens as i64).bind(report.context_window as i64)
            .bind(report.breakdown.to_string()).bind(aionui_common::now_ms()).bind(report.billing_reported).bind(report.context_reported).bind(serde_json::to_string(&report.request_metrics).unwrap_or_default())
            .bind(conversation).bind(user).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn query(&self, user: &str, q: &UsageQuery) -> Result<super::UsageResponse, DbError> {
        let (start, end) = q.period().map_err(|e| DbError::Conflict(e.into()))?;
        let rows=sqlx::query("SELECT strftime('%Y-%m-%d',created_at/1000,'unixepoch') AS period,provider,COALESCE((SELECT p.name FROM providers p WHERE p.id=usage_requests.provider AND p.user_id=usage_requests.user_id),(SELECT a.name FROM agent_metadata a WHERE a.agent_id=usage_requests.provider AND (a.user_id IS NULL OR a.user_id=usage_requests.user_id)),provider) AS provider_name,model,SUM(input_tokens) AS input_tokens,SUM(output_tokens) AS output_tokens,SUM(cache_read_tokens) AS cache_read_tokens,SUM(cache_creation_tokens) AS cache_creation_tokens,COUNT(*) AS requests FROM usage_requests WHERE billing_reported=1 AND user_id=? AND created_at>=? AND created_at<? AND (? IS NULL OR conversation_id=?) AND (? IS NULL OR provider=?) AND (? IS NULL OR model=?) GROUP BY period,provider,model ORDER BY period DESC,provider,model")
            .bind(user).bind(start).bind(end).bind(&q.conversation_id).bind(&q.conversation_id).bind(&q.provider).bind(&q.provider).bind(&q.model).bind(&q.model).fetch_all(&self.pool).await?;
        let mut totals = [0_i64; 5];
        let series:Vec<_>=rows.iter().map(|r| {
            let values=[r.get::<i64,_>("input_tokens"),r.get("output_tokens"),r.get("cache_read_tokens"),r.get("cache_creation_tokens"),r.get("requests")];
            for (total,value) in totals.iter_mut().zip(values) { *total=total.saturating_add(value); }
            serde_json::json!({"period":r.get::<String,_>("period"),"provider":r.get::<String,_>("provider"),"provider_name":r.get::<String,_>("provider_name"),"model":r.get::<String,_>("model"),"input_tokens":values[0],"output_tokens":values[1],"cache_read_tokens":values[2],"cache_creation_tokens":values[3],"requests":values[4]})
        }).collect();
        let contexts=sqlx::query("SELECT * FROM (SELECT conversation_id,COALESCE((SELECT c.name FROM conversations c WHERE c.id=usage_requests.conversation_id AND c.user_id=usage_requests.user_id),conversation_id) AS conversation_name,provider,model,COALESCE((SELECT p.name FROM providers p WHERE p.id=usage_requests.provider AND p.user_id=usage_requests.user_id),(SELECT a.name FROM agent_metadata a WHERE a.agent_id=usage_requests.provider AND (a.user_id IS NULL OR a.user_id=usage_requests.user_id)),provider) AS provider_name,context_tokens,context_window,breakdown,request_metrics,created_at,ROW_NUMBER() OVER(PARTITION BY conversation_id ORDER BY created_at DESC,rowid DESC) AS n FROM usage_requests WHERE context_reported=1 AND is_child=0 AND user_id=? AND created_at>=? AND created_at<? AND (? IS NULL OR conversation_id=?) AND (? IS NULL OR provider=?) AND (? IS NULL OR model=?)) WHERE n=1 ORDER BY created_at DESC")
            .bind(user).bind(start).bind(end).bind(&q.conversation_id).bind(&q.conversation_id).bind(&q.provider).bind(&q.provider).bind(&q.model).bind(&q.model).fetch_all(&self.pool).await?;
        let contexts:Vec<_>=contexts.iter().map(|r| serde_json::json!({"conversation_id":r.get::<String,_>("conversation_id"),"conversation_name":r.get::<String,_>("conversation_name"),"provider":r.get::<String,_>("provider"),"provider_name":r.get::<String,_>("provider_name"),"model":r.get::<String,_>("model"),"context_tokens":r.get::<i64,_>("context_tokens"),"context_window":r.get::<i64,_>("context_window"),"breakdown":serde_json::from_str::<serde_json::Value>(&r.get::<String,_>("breakdown")).unwrap_or_default(),"request_metrics":serde_json::from_str::<serde_json::Value>(&r.get::<String,_>("request_metrics")).unwrap_or_default(),"created_at":r.get::<i64,_>("created_at")})).collect();
        serde_json::from_value(serde_json::json!({"start":start,"end":end,"totals":{"input_tokens":totals[0],"output_tokens":totals[1],"cache_read_tokens":totals[2],"cache_creation_tokens":totals[3],"requests":totals[4]},"series":series,"contexts":contexts})).map_err(|e|DbError::Init(e.to_string()))
    }
}
