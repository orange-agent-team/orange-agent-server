//! Wait until the test request is actually queued, then release the fixture at its deadline.
pub async fn release_at_deadline(
    pool: &sqlx::PgPool,
    mut blocker: sqlx::Transaction<'_, sqlx::Postgres>,
    deadline: i64,
) {
    use std::time::Duration;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .expect("blocker PID");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let queued: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))"
            ).bind(pid).fetch_one(pool).await.expect("lock wait observation");
            if queued { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("request queued at the authority lock");
    sqlx::query("SELECT pg_sleep_until(to_timestamp($1::bigint) + interval '10 milliseconds')")
        .bind(deadline)
        .execute(&mut *blocker)
        .await
        .expect("fixture deadline");
    blocker.commit().await.expect("release fixture");
}
