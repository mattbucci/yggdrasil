use crate::db::DbPool;
use chrono::Utc;

use super::classifier::TaskCategory;
use super::collector::TokenUsage;

/// Write aggregated stats to the agent_stats table.
pub async fn record_stats(
    pool: &DbPool,
    agent_id: &str,
    usage: &TokenUsage,
    category: &TaskCategory,
) -> Result<(), sqlx::Error> {
    // Truncate to current hour for period bucketing
    let now = Utc::now();
    let period = now
        .date_naive()
        .and_hms_opt(now.time().hour(), 0, 0)
        .unwrap()
        .and_utc();

    sqlx::query(
        r#"
        INSERT INTO agent_stats (agent_id, period, input_tokens, output_tokens,
                                  cache_read, cache_write, tool_calls, task_category, estimated_cost)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (agent_id, period, task_category)
        DO UPDATE SET
            input_tokens = agent_stats.input_tokens + EXCLUDED.input_tokens,
            output_tokens = agent_stats.output_tokens + EXCLUDED.output_tokens,
            cache_read = agent_stats.cache_read + EXCLUDED.cache_read,
            cache_write = agent_stats.cache_write + EXCLUDED.cache_write,
            tool_calls = agent_stats.tool_calls + EXCLUDED.tool_calls,
            estimated_cost = agent_stats.estimated_cost + EXCLUDED.estimated_cost
        "#,
    )
    .bind(agent_id)
    .bind(period)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_read as i64)
    .bind(usage.cache_write as i64)
    .bind(1i32)
    .bind(category.to_string())
    .bind(estimate_cost(usage))
    .execute(pool)
    .await?;

    Ok(())
}

/// Write stats with REPLACE semantics — overwrites the current hour's row
/// with the running aggregate instead of adding to it. Safe to call on every
/// PromptSubmit turn without double-counting.
pub async fn replace_stats(
    pool: &DbPool,
    agent_id: &str,
    usage: &TokenUsage,
    category: &TaskCategory,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let period = now
        .date_naive()
        .and_hms_opt(now.time().hour(), 0, 0)
        .unwrap()
        .and_utc();

    sqlx::query(
        r#"
        INSERT INTO agent_stats (agent_id, period, input_tokens, output_tokens,
                                  cache_read, cache_write, tool_calls, task_category, estimated_cost)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (agent_id, period, task_category)
        DO UPDATE SET
            input_tokens = EXCLUDED.input_tokens,
            output_tokens = EXCLUDED.output_tokens,
            cache_read = EXCLUDED.cache_read,
            cache_write = EXCLUDED.cache_write,
            tool_calls = EXCLUDED.tool_calls,
            estimated_cost = EXCLUDED.estimated_cost
        "#,
    )
    .bind(agent_id)
    .bind(period)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_read as i64)
    .bind(usage.cache_write as i64)
    .bind(1i32)
    .bind(category.to_string())
    .bind(estimate_cost(usage))
    .execute(pool)
    .await?;

    Ok(())
}

/// Estimate cost in USD based on Claude Sonnet pricing.
/// Input: $3/MTok, Output: $15/MTok, Cache read: $0.30/MTok, Cache write: $3.75/MTok
fn estimate_cost(usage: &TokenUsage) -> f64 {
    let input = usage.input_tokens as f64 * 3.0 / 1_000_000.0;
    let output = usage.output_tokens as f64 * 15.0 / 1_000_000.0;
    let cache_r = usage.cache_read as f64 * 0.30 / 1_000_000.0;
    let cache_w = usage.cache_write as f64 * 3.75 / 1_000_000.0;
    input + output + cache_r + cache_w
}

/// Query total stats for an agent.
pub async fn get_agent_totals(pool: &DbPool, agent_id: &str) -> Result<AgentTotals, sqlx::Error> {
    let row = sqlx::query_as::<_, AgentTotals>(
        r#"
        SELECT COALESCE(SUM(input_tokens), 0) AS input_tokens,
               COALESCE(SUM(output_tokens), 0) AS output_tokens,
               COALESCE(SUM(cache_read), 0) AS cache_read,
               COALESCE(SUM(cache_write), 0) AS cache_write,
               COALESCE(SUM(tool_calls), 0) AS tool_calls,
               COALESCE(SUM(estimated_cost), 0.0) AS estimated_cost
        FROM agent_stats WHERE agent_id = $1
        "#,
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

#[derive(Debug, sqlx::FromRow)]
pub struct AgentTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub tool_calls: i32,
    pub estimated_cost: f64,
}

use chrono::Timelike;
