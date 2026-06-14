use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "task_status", rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    InProgress,
    Blocked,
    Closed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Blocked => write!(f, "blocked"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "in_progress" | "in-progress" | "wip" => Ok(Self::InProgress),
            "blocked" => Ok(Self::Blocked),
            "closed" | "done" => Ok(Self::Closed),
            _ => Err(format!("unknown status: {s}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "task_kind", rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Task,
    Bug,
    Feature,
    Chore,
    Epic,
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task => write!(f, "task"),
            Self::Bug => write!(f, "bug"),
            Self::Feature => write!(f, "feature"),
            Self::Chore => write!(f, "chore"),
            Self::Epic => write!(f, "epic"),
        }
    }
}

impl std::str::FromStr for TaskKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task" => Ok(Self::Task),
            "bug" => Ok(Self::Bug),
            "feature" | "feat" => Ok(Self::Feature),
            "chore" => Ok(Self::Chore),
            "epic" => Ok(Self::Epic),
            _ => Err(format!("unknown kind: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Task {
    pub task_id: Uuid,
    pub repo_id: Uuid,
    pub seq: i32,
    pub title: String,
    pub description: String,
    pub acceptance: Option<String>,
    pub design: Option<String>,
    pub notes: Option<String>,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub priority: i16,
    pub created_by: Option<Uuid>,
    pub assignee: Option<Uuid>,
    pub human_flag: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub close_reason: Option<String>,
    #[sqlx(default)]
    pub relevance: i32,
    #[sqlx(default)]
    pub external_ref: Option<String>,
}

#[derive(Debug, Default)]
pub struct TaskCreate<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub acceptance: Option<&'a str>,
    pub design: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub kind: TaskKind,
    pub priority: i16,
    pub assignee: Option<Uuid>,
    pub labels: &'a [String],
    pub external_ref: Option<&'a str>,
}

#[derive(Debug, Default)]
pub struct TaskUpdate<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub acceptance: Option<&'a str>,
    pub design: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub kind: Option<TaskKind>,
    pub priority: Option<i16>,
    pub assignee: Option<Option<Uuid>>, // Some(Some(id)) sets; Some(None) clears; None leaves alone
    pub human_flag: Option<bool>,
    pub external_ref: Option<Option<&'a str>>, // Some(Some(s)) sets; Some(None) clears; None leaves alone
}

pub struct TaskRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> TaskRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Atomically allocate the next per-repo sequence number.
    async fn next_seq(
        &self,
        repo_id: Uuid,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar::<_, i32>(
            r#"INSERT INTO task_seq (repo_id, next_seq)
               VALUES ($1, 2)
               ON CONFLICT (repo_id) DO UPDATE
                   SET next_seq = task_seq.next_seq + 1
               RETURNING next_seq - 1"#,
        )
        .bind(repo_id)
        .fetch_one(&mut **tx)
        .await
    }

    pub async fn create(
        &self,
        repo_id: Uuid,
        created_by: Option<Uuid>,
        spec: TaskCreate<'_>,
    ) -> Result<Task, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let seq = self.next_seq(repo_id, &mut tx).await?;

        // ADR 0016 / yggdrasil-105: new tasks of kind task|bug|feature|chore
        // default to runnable=TRUE so the scheduler picks them up. Epics stay
        // manual unless they opt in via plan_strategy='llm' (set later).
        let runnable_default = !matches!(spec.kind, TaskKind::Epic);

        let task: Task = sqlx::query_as::<_, Task>(
            r#"INSERT INTO tasks
               (repo_id, seq, title, description, acceptance, design, notes,
                kind, priority, created_by, assignee, external_ref, runnable)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
               RETURNING task_id, repo_id, seq, title, description, acceptance, design, notes,
                         kind, status, priority, created_by, assignee, human_flag,
                         created_at, updated_at, closed_at, close_reason, relevance, external_ref"#,
        )
        .bind(repo_id)
        .bind(seq)
        .bind(spec.title)
        .bind(spec.description)
        .bind(spec.acceptance)
        .bind(spec.design)
        .bind(spec.notes)
        .bind(&spec.kind)
        .bind(spec.priority)
        .bind(created_by)
        .bind(spec.assignee)
        .bind(spec.external_ref)
        .bind(runnable_default)
        .fetch_one(&mut *tx)
        .await?;

        for label in spec.labels {
            sqlx::query(
                "INSERT INTO task_labels (task_id, label) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(task.task_id)
            .bind(label)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "INSERT INTO task_events (task_id, agent_id, kind, payload) VALUES ($1, $2, 'created', $3)",
        )
        .bind(task.task_id)
        .bind(created_by)
        .bind(serde_json::json!({ "title": spec.title }))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(task)
    }

    pub async fn get(&self, task_id: Uuid) -> Result<Option<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance, external_ref
               FROM tasks WHERE task_id = $1"#,
        )
        .bind(task_id)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_ref(&self, repo_id: Uuid, seq: i32) -> Result<Option<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance, external_ref
               FROM tasks WHERE repo_id = $1 AND seq = $2"#,
        )
        .bind(repo_id)
        .bind(seq)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn list(
        &self,
        repo_id: Option<Uuid>,
        status: Option<TaskStatus>,
    ) -> Result<Vec<Task>, sqlx::Error> {
        self.list_multi(repo_id, status.map(|s| vec![s]).as_deref())
            .await
    }

    /// Filter by multiple statuses at once (`open,in_progress`). An empty or
    /// None slice means "any status".
    pub async fn list_multi(
        &self,
        repo_id: Option<Uuid>,
        statuses: Option<&[TaskStatus]>,
    ) -> Result<Vec<Task>, sqlx::Error> {
        let status_strs: Vec<String> = statuses
            .map(|s| s.iter().map(|st| st.to_string()).collect())
            .unwrap_or_default();
        sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance, external_ref
               FROM tasks
               WHERE ($1::UUID IS NULL OR repo_id = $1)
                 AND ($2::text[] IS NULL OR array_length($2, 1) IS NULL OR status::text = ANY($2))
                 AND deleted_at IS NULL
               ORDER BY status, priority, seq"#,
        )
        .bind(repo_id)
        .bind(if status_strs.is_empty() {
            None
        } else {
            Some(status_strs)
        })
        .fetch_all(self.pool)
        .await
    }

    /// Return all open/in-progress tasks in the given repo that have no
    /// unsatisfied blockers. Ordered by priority, then seq.
    pub async fn ready(&self, repo_id: Uuid) -> Result<Vec<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"
            SELECT t.task_id, t.repo_id, t.seq, t.title, t.description, t.acceptance, t.design, t.notes,
                   t.kind, t.status, t.priority, t.created_by, t.assignee, t.human_flag,
                   t.created_at, t.updated_at, t.closed_at, t.close_reason, t.relevance, t.external_ref
            FROM tasks t
            WHERE t.repo_id = $1
              AND t.status IN ('open', 'in_progress')
              AND t.deleted_at IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM task_deps d
                  JOIN tasks b ON b.task_id = d.blocker_id
                  WHERE d.task_id = t.task_id
                    AND b.status <> 'closed'
                    AND b.deleted_at IS NULL
              )
            ORDER BY t.priority, t.seq
            "#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
    }

    /// Tasks not updated in `>= days` days, still open/in_progress/blocked.
    /// Pass `repo_id = None` to scan every repo. Useful for triage of abandoned
    /// claims — particularly `status = 'in_progress'` rows whose updated_at
    /// has gone quiet.
    pub async fn stale(&self, repo_id: Option<Uuid>, days: i32) -> Result<Vec<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"
            SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                   kind, status, priority, created_by, assignee, human_flag,
                   created_at, updated_at, closed_at, close_reason, relevance, external_ref
            FROM tasks
            WHERE ($1::UUID IS NULL OR repo_id = $1)
              AND status <> 'closed'
              AND deleted_at IS NULL
              AND updated_at < now() - make_interval(days => $2)
            ORDER BY updated_at ASC, priority, seq
            "#,
        )
        .bind(repo_id)
        .bind(days)
        .fetch_all(self.pool)
        .await
    }

    pub async fn blocked(&self, repo_id: Uuid) -> Result<Vec<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"
            SELECT DISTINCT t.task_id, t.repo_id, t.seq, t.title, t.description, t.acceptance, t.design, t.notes,
                   t.kind, t.status, t.priority, t.created_by, t.assignee, t.human_flag,
                   t.created_at, t.updated_at, t.closed_at, t.close_reason, t.relevance, t.external_ref
            FROM tasks t
            JOIN task_deps d ON d.task_id = t.task_id
            JOIN tasks b ON b.task_id = d.blocker_id
            WHERE t.repo_id = $1
              AND t.status <> 'closed'
              AND b.status <> 'closed'
              AND t.deleted_at IS NULL
              AND b.deleted_at IS NULL
            ORDER BY t.priority, t.seq
            "#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn set_status(
        &self,
        task_id: Uuid,
        status: TaskStatus,
        agent_id: Option<Uuid>,
        close_reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let closed_at = matches!(status, TaskStatus::Closed).then(Utc::now);

        sqlx::query(
            r#"UPDATE tasks
               SET status = $2,
                   closed_at = CASE WHEN $2 = 'closed' THEN COALESCE(closed_at, now()) ELSE NULL END,
                   close_reason = CASE WHEN $2 = 'closed' THEN $3 ELSE NULL END,
                   updated_at = now()
               WHERE task_id = $1"#,
        )
        .bind(task_id)
        .bind(&status)
        .bind(close_reason)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO task_events (task_id, agent_id, kind, payload) VALUES ($1, $2, 'status_change', $3)",
        )
        .bind(task_id)
        .bind(agent_id)
        .bind(serde_json::json!({
            "to": status.to_string(),
            "closed_at": closed_at,
            "close_reason": close_reason
        }))
        .execute(&mut *tx)
        .await?;

        tx.commit().await
    }

    pub async fn update(
        &self,
        task_id: Uuid,
        agent_id: Option<Uuid>,
        u: TaskUpdate<'_>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        if let Some(v) = u.title {
            sqlx::query("UPDATE tasks SET title = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.description {
            sqlx::query("UPDATE tasks SET description = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.acceptance {
            sqlx::query("UPDATE tasks SET acceptance = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.design {
            sqlx::query("UPDATE tasks SET design = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.notes {
            sqlx::query("UPDATE tasks SET notes = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.kind {
            sqlx::query("UPDATE tasks SET kind = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(&v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.priority {
            sqlx::query("UPDATE tasks SET priority = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.assignee {
            sqlx::query("UPDATE tasks SET assignee = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.human_flag {
            sqlx::query("UPDATE tasks SET human_flag = $2, updated_at = now() WHERE task_id = $1")
                .bind(task_id)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(v) = u.external_ref {
            // Some(Some(s)) sets; Some(None) clears; None leaves alone.
            sqlx::query(
                "UPDATE tasks SET external_ref = $2, updated_at = now() WHERE task_id = $1",
            )
            .bind(task_id)
            .bind(v)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "INSERT INTO task_events (task_id, agent_id, kind, payload) VALUES ($1, $2, 'updated', '{}'::jsonb)",
        )
        .bind(task_id).bind(agent_id).execute(&mut *tx).await?;

        tx.commit().await
    }

    /// Reassign a task to another repo, allocating a fresh per-repo sequence in
    /// the destination (the human ref changes, e.g. `yggdrasil-42` → `canairy-7`;
    /// the task_id is stable so deps, links, labels, and events follow). The old
    /// seq in the source repo is not reclaimed — gaps are expected. Returns the
    /// new seq. No-op guard against moving into the same repo lives in the CLI.
    pub async fn move_to_repo(
        &self,
        task_id: Uuid,
        target_repo_id: Uuid,
        agent_id: Option<Uuid>,
    ) -> Result<i32, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let new_seq = self.next_seq(target_repo_id, &mut tx).await?;

        sqlx::query(
            "UPDATE tasks SET repo_id = $2, seq = $3, updated_at = now() WHERE task_id = $1",
        )
        .bind(task_id)
        .bind(target_repo_id)
        .bind(new_seq)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO task_events (task_id, agent_id, kind, payload) VALUES ($1, $2, 'moved', $3)",
        )
        .bind(task_id)
        .bind(agent_id)
        .bind(serde_json::json!({ "repo_id": target_repo_id, "seq": new_seq }))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(new_seq)
    }

    pub async fn add_dep(&self, task_id: Uuid, blocker_id: Uuid) -> Result<(), sqlx::Error> {
        if task_id == blocker_id {
            return Err(sqlx::Error::Protocol("task cannot depend on itself".into()));
        }
        // Naive cycle check: does reachable(blocker_id) include task_id?
        let cycle = sqlx::query_scalar::<_, bool>(
            r#"
            WITH RECURSIVE reachable(tid) AS (
                SELECT $1::UUID
                UNION
                SELECT d.blocker_id FROM task_deps d JOIN reachable r ON r.tid = d.task_id
            )
            SELECT EXISTS (SELECT 1 FROM reachable WHERE tid = $2)
            "#,
        )
        .bind(blocker_id)
        .bind(task_id)
        .fetch_one(self.pool)
        .await?;
        if cycle {
            return Err(sqlx::Error::Protocol(
                "would create a dependency cycle".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO task_deps (task_id, blocker_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(task_id)
        .bind(blocker_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Walk task_deps following blocker edges and surface every cycle the
    /// graph already contains. Each returned vec is one cycle in order
    /// (without the duplicate closing vertex), canonicalized so its smallest
    /// UUID appears first; identical rotations of the same cycle dedupe.
    pub async fn find_cycles(&self, repo_id: Option<Uuid>) -> Result<Vec<Vec<Uuid>>, sqlx::Error> {
        let rows: Vec<(Vec<Uuid>,)> = sqlx::query_as(
            r#"
            WITH RECURSIVE walk(start_tid, tid, path, has_cycle) AS (
                SELECT d.task_id, d.blocker_id,
                       ARRAY[d.task_id, d.blocker_id], FALSE
                FROM task_deps d
                JOIN tasks t ON t.task_id = d.task_id
                WHERE $1::UUID IS NULL OR t.repo_id = $1
                UNION ALL
                SELECT w.start_tid, d.blocker_id,
                       w.path || d.blocker_id,
                       d.blocker_id = ANY(w.path)
                FROM walk w
                JOIN task_deps d ON d.task_id = w.tid
                WHERE NOT w.has_cycle AND array_length(w.path, 1) < 100
            )
            SELECT path FROM walk WHERE has_cycle
            "#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await?;

        let mut seen: std::collections::HashSet<Vec<Uuid>> = Default::default();
        let mut cycles: Vec<Vec<Uuid>> = Vec::new();
        for (path,) in rows {
            // Path comes back as [v0, v1, ..., vk] where vk == some earlier vi.
            // Slice to the cycle proper: [vi, v_{i+1}, ..., v_{k-1}].
            let last = match path.last() {
                Some(u) => *u,
                None => continue,
            };
            let start_idx = match path.iter().position(|u| *u == last) {
                Some(i) => i,
                None => continue,
            };
            if start_idx + 1 >= path.len() {
                continue;
            }
            let cycle = &path[start_idx..path.len() - 1];
            if cycle.is_empty() {
                continue;
            }
            // Canonicalize: rotate so the smallest UUID is first.
            let min_idx = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, u)| *u)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let mut canon = cycle[min_idx..].to_vec();
            canon.extend_from_slice(&cycle[..min_idx]);
            if seen.insert(canon.clone()) {
                cycles.push(canon);
            }
        }
        cycles.sort();
        Ok(cycles)
    }

    pub async fn remove_dep(&self, task_id: Uuid, blocker_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM task_deps WHERE task_id = $1 AND blocker_id = $2")
            .bind(task_id)
            .bind(blocker_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Find probable duplicate pairs using token-set Jaccard similarity on
    /// title+description — no embeddings (ADR 0015). Returns pairs
    /// `(older, newer, similarity)` with similarity >= `min_similarity`,
    /// sorted most-similar first, capped at `limit`. Scoped to `repo_id`,
    /// or global when None. Open, non-deleted tasks only.
    pub async fn find_dupes(
        &self,
        repo_id: Option<Uuid>,
        min_similarity: f64,
        limit: i64,
    ) -> Result<Vec<(Task, Task, f64)>, sqlx::Error> {
        let tasks: Vec<Task> = sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance, external_ref
               FROM tasks
               WHERE ($1::UUID IS NULL OR repo_id = $1)
                 AND status <> 'closed'
                 AND deleted_at IS NULL
               ORDER BY created_at"#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await?;

        // Precompute a token set per task.
        let token_sets: Vec<std::collections::HashSet<String>> = tasks
            .iter()
            .map(|t| tokenize(&format!("{} {}", t.title, t.description)))
            .collect();

        // Inverted index: token -> task indices. Two tasks with no shared
        // token have Jaccard 0, so only pairs that co-occur on some token
        // can clear the threshold. Comparing just those candidate pairs
        // avoids the all-pairs O(n²) sweep on disjoint tasks.
        let mut by_token: std::collections::HashMap<&str, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, set) in token_sets.iter().enumerate() {
            for tok in set {
                by_token.entry(tok.as_str()).or_default().push(i);
            }
        }
        // A token appearing in more than `df_cap` tasks is non-discriminative
        // (a near stop-word for this corpus) and its bucket would reintroduce
        // the O(n²) sweep. Skip it as a blocking key: a pair sharing only such
        // ubiquitous tokens has a low Jaccard and never clears the threshold,
        // while a pair sharing any rarer token is still compared via that
        // token's bucket. Floored so small corpora are unaffected.
        let df_cap = (tasks.len() / 10).max(64);
        let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
        let mut pairs: Vec<(Task, Task, f64)> = Vec::new();
        for idxs in by_token.values() {
            if idxs.len() > df_cap {
                continue;
            }
            for a in 0..idxs.len() {
                for b in (a + 1)..idxs.len() {
                    // tasks are ordered by created_at, so the smaller index
                    // is the older task; normalize so each pair is unique.
                    let (i, j) = (idxs[a].min(idxs[b]), idxs[a].max(idxs[b]));
                    if !seen.insert((i, j)) {
                        continue;
                    }
                    let sim = jaccard(&token_sets[i], &token_sets[j]);
                    if sim >= min_similarity {
                        pairs.push((tasks[i].clone(), tasks[j].clone(), sim));
                    }
                }
            }
        }
        pairs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        if limit > 0 {
            pairs.truncate(limit as usize);
        }
        Ok(pairs)
    }

    pub async fn add_label(&self, task_id: Uuid, label: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO task_labels (task_id, label) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(task_id)
        .bind(label)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_label(&self, task_id: Uuid, label: &str) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM task_labels WHERE task_id = $1 AND label = $2")
            .bind(task_id)
            .bind(label)
            .execute(self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Every distinct label across every task in a repo (or globally if None),
    /// with usage count. Backs `ygg task label list-all` and label-picker UIs.
    pub async fn all_labels(
        &self,
        repo_id: Option<Uuid>,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as::<_, (String, i64)>(
            r#"SELECT tl.label, COUNT(*)::bigint
               FROM task_labels tl
               JOIN tasks t USING (task_id)
               WHERE $1::UUID IS NULL OR t.repo_id = $1
               GROUP BY tl.label
               ORDER BY COUNT(*) DESC, tl.label"#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
    }

    /// Adjust relevance by a delta; clamped to 0..100.
    pub async fn bump_relevance(&self, task_id: Uuid, delta: i32) -> Result<i32, sqlx::Error> {
        let new_val: i32 = sqlx::query_scalar(
            r#"UPDATE tasks
                  SET relevance = GREATEST(0, LEAST(100, relevance + $2)),
                      updated_at = now()
                WHERE task_id = $1
                RETURNING relevance"#,
        )
        .bind(task_id)
        .bind(delta)
        .fetch_one(self.pool)
        .await?;
        Ok(new_val)
    }

    pub async fn add_link(
        &self,
        task_id: Uuid,
        target_id: Uuid,
        kind: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO task_links (task_id, target_id, kind)
             VALUES ($1, $2, $3::task_link_kind)
             ON CONFLICT DO NOTHING",
        )
        .bind(task_id)
        .bind(target_id)
        .bind(kind)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn links(&self, task_id: Uuid) -> Result<Vec<(String, Uuid)>, sqlx::Error> {
        sqlx::query_as::<_, (String, Uuid)>(
            "SELECT kind::text, target_id FROM task_links WHERE task_id = $1 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn labels(&self, task_id: Uuid) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT label FROM task_labels WHERE task_id = $1 ORDER BY label",
        )
        .bind(task_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn deps(&self, task_id: Uuid) -> Result<Vec<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"SELECT t.task_id, t.repo_id, t.seq, t.title, t.description, t.acceptance, t.design, t.notes,
                      t.kind, t.status, t.priority, t.created_by, t.assignee, t.human_flag,
                      t.created_at, t.updated_at, t.closed_at, t.close_reason, t.relevance, t.external_ref
               FROM task_deps d JOIN tasks t ON t.task_id = d.blocker_id
               WHERE d.task_id = $1
               ORDER BY t.seq"#,
        )
        .bind(task_id).fetch_all(self.pool).await
    }

    /// Hard-delete a task. FK cascades handle task_deps, task_events,
    /// task_relevance, and worker rows — no manual cleanup needed.
    pub async fn delete(&self, task_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM tasks WHERE task_id = $1")
            .bind(task_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Soft-delete a task: stamp deleted_at = now() so list/ready/blocked
    /// stop returning it. Already-deleted rows are touched again so the
    /// 30-day purge clock resets — useful when a user restores then
    /// re-deletes. Returns whether the row exists.
    pub async fn soft_delete(&self, task_id: Uuid) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            "UPDATE tasks SET deleted_at = now(), updated_at = now() WHERE task_id = $1",
        )
        .bind(task_id)
        .execute(self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Pull a task back from the trash. Returns whether anything changed —
    /// `false` for a missing row or a row that was never deleted.
    pub async fn restore(&self, task_id: Uuid) -> Result<bool, sqlx::Error> {
        let n = sqlx::query(
            r#"UPDATE tasks
               SET deleted_at = NULL,
                   updated_at = now()
               WHERE task_id = $1 AND deleted_at IS NOT NULL"#,
        )
        .bind(task_id)
        .execute(self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// List tasks that are in the trash. Scoped to `repo_id`, or global
    /// when None. Newest-trashed first so a human reviewing what to restore
    /// or purge sees recent moves at the top.
    pub async fn list_trashed(&self, repo_id: Option<Uuid>) -> Result<Vec<Task>, sqlx::Error> {
        sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance, external_ref
               FROM tasks
               WHERE deleted_at IS NOT NULL
                 AND ($1::UUID IS NULL OR repo_id = $1)
               ORDER BY deleted_at DESC, seq"#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
    }

    /// Hard-delete every trashed row whose `deleted_at` is older than
    /// `older_than_days`. Returns the number of rows removed. FK cascades
    /// take care of dependent task_deps / task_events / etc rows.
    pub async fn purge_older_than(
        &self,
        older_than_days: i32,
        repo_id: Option<Uuid>,
    ) -> Result<u64, sqlx::Error> {
        let n = sqlx::query(
            r#"DELETE FROM tasks
               WHERE deleted_at IS NOT NULL
                 AND deleted_at < now() - make_interval(days => $1)
                 AND ($2::UUID IS NULL OR repo_id = $2)"#,
        )
        .bind(older_than_days)
        .bind(repo_id)
        .execute(self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    pub async fn stats(&self, repo_id: Option<Uuid>) -> Result<TaskStats, sqlx::Error> {
        let row: (i64, i64, i64, i64) = sqlx::query_as(
            r#"SELECT
                   COUNT(*) FILTER (WHERE status = 'open'),
                   COUNT(*) FILTER (WHERE status = 'in_progress'),
                   COUNT(*) FILTER (WHERE status = 'blocked'),
                   COUNT(*) FILTER (WHERE status = 'closed')
               FROM tasks
               WHERE $1::UUID IS NULL OR repo_id = $1"#,
        )
        .bind(repo_id)
        .fetch_one(self.pool)
        .await?;
        Ok(TaskStats {
            open: row.0,
            in_progress: row.1,
            blocked: row.2,
            closed: row.3,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskStats {
    pub open: i64,
    pub in_progress: i64,
    pub blocked: i64,
    pub closed: i64,
}

/// Lowercase alphanumeric word tokens, dropping very short noise tokens.
/// Used by `find_dupes` for embedding-free similarity (ADR 0015).
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Jaccard similarity of two token sets: |A∩B| / |A∪B|. Returns 0.0 when
/// both sets are empty.
fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}
