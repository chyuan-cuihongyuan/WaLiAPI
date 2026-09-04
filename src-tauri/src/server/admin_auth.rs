//! Web 管理面板认证：admin_users 表 + argon2id 哈希 + Bearer token session。

use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use dashmap::DashMap;
use password_hash::rand_core::OsRng;
use rand::{distr::Alphanumeric, Rng};
use sqlx::SqlitePool;

const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// 过期会话清扫的最小间隔：插入路径顺带清扫，每小时至多一次
/// （会话量小，纯粹是防长驻进程里失效条目无限滞留）。
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Clone)]
pub struct AdminSession {
    pub user_id: String,
    pub username: String,
    pub must_change_password: bool,
}

pub struct SessionStore {
    inner: DashMap<String, (AdminSession, Instant)>,
    last_sweep: std::sync::Mutex<Instant>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            inner: DashMap::new(),
            last_sweep: std::sync::Mutex::new(Instant::now()),
        }
    }
}

impl SessionStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, token: String, session: AdminSession) {
        // 顺带清扫过期会话（时间门控，避免高频登录下的重复全表扫描）。
        if let Ok(mut last) = self.last_sweep.lock() {
            if last.elapsed() >= SESSION_SWEEP_INTERVAL {
                self.sweep_expired();
                *last = Instant::now();
            }
        }
        self.inner.insert(token, (session, Instant::now() + SESSION_TTL));
    }

    pub fn get(&self, token: &str) -> Option<AdminSession> {
        let entry = self.inner.get(token)?;
        let (session, expiry) = entry.value();
        if Instant::now() > *expiry {
            drop(entry);
            self.inner.remove(token);
            return None;
        }
        Some(session.clone())
    }

    pub fn remove(&self, token: &str) {
        self.inner.remove(token);
    }

    /// 吊销某用户的全部会话（FIX-17：改密后旧会话一律失效）。
    pub fn revoke_all_for_user(&self, user_id: &str) {
        self.inner.retain(|_, (session, _)| session.user_id != user_id);
    }

    /// 清扫全部过期会话（FIX-17：不依赖逐条 get 才惰性删除）。
    pub fn sweep_expired(&self) {
        let now = Instant::now();
        self.inner.retain(|_, (_, expiry)| *expiry > now);
    }
}

/// 登录失败限速（FIX-17）：按用户名与全局两级计数 + 指数退避。
/// 内存实现、进程重启清零——管理面单实例部署，无跨进程共享需求。
///
/// 计数带时间窗口（`FAIL_MEMORY`）：窗口外的旧失败不再累积；
/// 退避时长自最后一次失败起算，随时间流逝自然解锁。
pub struct LoginThrottle {
    inner: DashMap<String, FailEntry>,
    last_prune: std::sync::Mutex<Instant>,
}

#[derive(Clone, Copy)]
struct FailEntry {
    /// 连续失败次数（窗口内）。
    failures: u32,
    /// 最后一次失败时刻；退避从该时刻起算。
    last_failure: Instant,
}

/// 全局计数的键（用户名不可能包含 NUL，无碰撞）。
pub const LOGIN_GLOBAL_KEY: &str = "\u{0}global";

/// 免退避的失败次数。
const LOGIN_FREE_FAILURES: u32 = 5;
/// 退避上限（2^(failures-5) 秒封顶）。
const LOGIN_MAX_LOCKOUT: Duration = Duration::from_secs(3600);
/// 失败计数的记忆窗口：窗口外的失败视为过期，不再触发退避。
const FAIL_MEMORY: Duration = Duration::from_secs(15 * 60);

impl Default for LoginThrottle {
    fn default() -> Self {
        Self {
            inner: DashMap::new(),
            last_prune: std::sync::Mutex::new(Instant::now()),
        }
    }
}

impl LoginThrottle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 当前 key 需继续等待的退避剩余时间；零表示放行。
    pub fn penalty_remaining(&self, key: &str, now: Instant) -> Duration {
        let entry = match self.inner.get(key) {
            Some(e) => *e.value(),
            None => return Duration::ZERO,
        };
        if now.duration_since(entry.last_failure) > FAIL_MEMORY {
            return Duration::ZERO;
        }
        backoff_for(entry.failures).saturating_sub(now.duration_since(entry.last_failure))
    }

    /// 记录一次失败（窗口外的旧计数先归零再累加）。
    pub fn record_failure(&self, key: &str, now: Instant) {
        let next = match self.inner.get(key) {
            Some(e) if now.duration_since(e.last_failure) <= FAIL_MEMORY => e.failures + 1,
            _ => 1,
        };
        self.inner
            .insert(key.to_string(), FailEntry { failures: next, last_failure: now });
        // 顺带修剪长期无活动的条目，防随机用户名撑爆计数表。
        if let Ok(mut last) = self.last_prune.lock() {
            if last.elapsed() >= FAIL_MEMORY {
                self.inner.retain(|_, e| now.duration_since(e.last_failure) <= FAIL_MEMORY);
                *last = now;
            }
        }
    }

    /// 登录成功即清零该 key 的失败计数（成功登录不受影响）。
    pub fn record_success(&self, key: &str) {
        self.inner.remove(key);
    }
}

/// 指数退避：前 `LOGIN_FREE_FAILURES` 次免罚，此后 2^(n-5) 秒，封顶 1 小时。
fn backoff_for(failures: u32) -> Duration {
    if failures <= LOGIN_FREE_FAILURES {
        return Duration::ZERO;
    }
    let exp = (failures - LOGIN_FREE_FAILURES).min(12); // 2^12s > 1h，防 u64 溢出
    Duration::from_secs(1u64 << exp).min(LOGIN_MAX_LOCKOUT)
}

/// 时序防用户名枚举（FIX-17）：用户名不存在时也执行一次真实 argon2 验证，
/// 让「用户不存在」与「密码错误」的响应耗时一致。哈希进程内随机生成一次。
static DUMMY_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn dummy_verify(password: &str) {
    let hash = DUMMY_HASH.get_or_init(|| {
        hash_password(&generate_token()).unwrap_or_else(|_| "$argon2id$invalid".to_string())
    });
    let _ = verify_password(password, hash);
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("密码哈希失败: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn generate_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn generate_password(len: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub struct AdminUserRow {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub must_change_password: bool,
}

pub async fn find_user_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<AdminUserRow>, String> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, username, password_hash, must_change_password FROM admin_users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(row.map(|(id, username, password_hash, mcp)| AdminUserRow {
        id,
        username,
        password_hash,
        must_change_password: mcp != 0,
    }))
}

pub async fn update_password(pool: &SqlitePool, user_id: &str, new_password: &str) -> Result<(), String> {
    let hash = hash_password(new_password)?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE admin_users SET password_hash = ?, must_change_password = 0, updated_at = ? WHERE id = ?")
        .bind(hash)
        .bind(now)
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn update_username(pool: &SqlitePool, user_id: &str, new_username: &str) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE admin_users SET username = ?, updated_at = ? WHERE id = ?")
        .bind(new_username)
        .bind(now)
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 首次启动时若无 admin 用户，创建 admin + 随机 16 位密码并写入 INITIAL_PASSWORD 文件。
/// `data_dir` 为应用数据目录（容器内 /data/<identifier>）。
pub async fn ensure_initial_admin(pool: &SqlitePool, data_dir: &std::path::Path) -> Result<(), String> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    if count > 0 {
        return Ok(());
    }

    let username = "admin";
    let password = generate_password(16);
    let hash = hash_password(&password)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO admin_users (id, username, password_hash, must_change_password, created_at, updated_at) VALUES (?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(username)
    .bind(&hash)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 临时密码必须出现在容器 stdout（docker logs），同时写入数据目录文件
    println!("==============================================");
    println!("WaLiAPI Web 管理面板初始账号: {username}");
    println!("初始临时密码: {password}");
    println!("请登录后立即修改密码。");
    println!("==============================================");

    // 写入数据目录，容器内为 /data/<identifier>/INITIAL_PASSWORD
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        log::warn!("创建数据目录失败: {e}");
    }
    let file = data_dir.join("INITIAL_PASSWORD");
    if let Err(e) = std::fs::write(&file, format!("username: {username}\npassword: {password}\n")) {
        log::warn!("写入 INITIAL_PASSWORD 失败: {e}");
    } else {
        log::info!("初始密码已写入 {}", file.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(user_id: &str) -> AdminSession {
        AdminSession {
            user_id: user_id.to_string(),
            username: "admin".to_string(),
            must_change_password: false,
        }
    }

    // ─── FIX-17：登录失败限速 ──────────────────────────────────────────────

    #[test]
    fn free_failures_do_not_lock_out() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for i in 0..LOGIN_FREE_FAILURES {
            throttle.record_failure("admin", t0 + Duration::from_secs(i as u64));
        }
        assert_eq!(
            throttle.penalty_remaining("admin", t0 + Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn consecutive_failures_back_off_exponentially_and_decay() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        // 5 次免罚后第 6/7/8 次 → 2s / 4s / 8s（最后失败发生在 t0+7）
        for i in 0..8 {
            throttle.record_failure("admin", t0 + Duration::from_secs(i as u64));
        }
        // 8s 退避自 t0+7 起算：t0+8 时剩 7s
        assert_eq!(
            throttle.penalty_remaining("admin", t0 + Duration::from_secs(8)),
            Duration::from_secs(7)
        );
        // 等满退避窗口后自然解锁
        assert_eq!(
            throttle.penalty_remaining("admin", t0 + Duration::from_secs(15)),
            Duration::ZERO
        );
    }

    #[test]
    fn success_login_resets_counter() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for i in 0..10 {
            throttle.record_failure("admin", t0 + Duration::from_secs(i as u64));
        }
        assert!(!throttle.penalty_remaining("admin", t0).is_zero());
        throttle.record_success("admin");
        assert_eq!(throttle.penalty_remaining("admin", t0), Duration::ZERO);
    }

    #[test]
    fn stale_failures_outside_memory_window_expire() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for i in 0..20 {
            throttle.record_failure("admin", t0 + Duration::from_secs(i as u64));
        }
        // 记忆窗口自最后一次失败（t0+19）起算：安静超过 15 分钟后，
        // 新失败从 1 重新计数（不累计成永久封禁）
        let later = t0 + Duration::from_secs(19) + FAIL_MEMORY + Duration::from_secs(1);
        throttle.record_failure("admin", later);
        assert_eq!(throttle.penalty_remaining("admin", later), Duration::ZERO);
    }

    #[test]
    fn per_user_counters_are_isolated_from_global() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        // 三个不同用户名各失败 4 次：单独不触发，也不污染其他用户
        for name in ["alice", "bob", "carol"] {
            for i in 0..4 {
                throttle.record_failure(name, t0 + Duration::from_secs(i as u64));
            }
        }
        assert_eq!(throttle.penalty_remaining("alice", t0), Duration::ZERO);
        assert_eq!(throttle.penalty_remaining("dave", t0), Duration::ZERO);
        assert_eq!(throttle.penalty_remaining(LOGIN_GLOBAL_KEY, t0), Duration::ZERO);
    }

    #[test]
    fn backoff_caps_at_one_hour() {
        assert_eq!(backoff_for(LOGIN_FREE_FAILURES + 12), LOGIN_MAX_LOCKOUT);
        assert_eq!(backoff_for(u32::MAX), LOGIN_MAX_LOCKOUT);
    }

    // ─── FIX-17：会话吊销与过期清扫 ────────────────────────────────────────

    #[test]
    fn revoke_all_for_user_drops_only_that_user() {
        let store = SessionStore::default();
        store.insert("t1".into(), session("u1"));
        store.insert("t2".into(), session("u2"));
        store.revoke_all_for_user("u1");
        assert!(store.get("t1").is_none());
        assert!(store.get("t2").is_some());
    }

    #[test]
    fn sweep_expired_removes_stale_sessions() {
        let store = SessionStore::default();
        store.insert("live".into(), session("u1"));
        // 同模块内可直接构造已过期条目（绕过 TTL 常量）
        store.inner.insert(
            "expired".into(),
            (session("u1"), Instant::now() - Duration::from_secs(1)),
        );
        store.sweep_expired();
        assert!(store.get("live").is_some());
        assert!(store.get("expired").is_none());
    }

    // ─── FIX-17：哑哈希不 panic、耗时可观 ──────────────────────────────────

    #[test]
    fn dummy_verify_runs_real_argon2_without_panic() {
        dummy_verify("whatever");
        dummy_verify("密钥密码");
    }
}
