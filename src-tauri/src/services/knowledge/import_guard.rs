//! 知识库导入安全守卫（FIX-07）：git / local_dir / url 三条导入路径的纯校验逻辑。
//!
//! - **git**：仅接受 `https://` scheme（`ext::` 等伪协议可触发命令执行）；URL 解析重建后
//!   使用（丢弃 userinfo/query/fragment）；凭证经 `git -c http.extraHeader=...` 注入而非
//!   拼 URL（不随 git stderr 回显）；branch 拒绝选项形态与空白；失败信息回传前剥离
//!   任何含凭证的片段并截断。
//! - **local_dir**：默认仅允许数据目录内；设置项 `kb.import.allowed_roots`（字符串数组）
//!   可显式扩展白名单根；canonicalize 解析符号链接后做前缀校验（符号链接逃逸在
//!   规范化后必然落到根外，从而被拒绝）。
//! - **url**：仅 http/https；DNS 解析后拒绝环回/私网/链路本地/唯一本地等内网目标
//!   （校验全部 A/AAAA 记录，防多条记录里夹带内网地址）；不跟随跨主机重定向；
//!   响应体大小上限由导入器执行。
//!
//! 本模块只做纯校验（DNS 解析除外），全部单测不发真实网络请求、不真 clone。

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use url::Url;

// ─── git ────────────────────────────────────────────────────────────

/// git 导入 URL 校验：仅 https、无空白/控制字符，解析后重建干净 URL
/// （丢弃 userinfo —— 凭证一律走 extraHeader；丢弃 query/fragment）。
pub fn normalize_git_url(repo_url: &str) -> Result<String, String> {
    if repo_url.trim() != repo_url
        || repo_url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("仓库地址不能包含空白或控制字符".to_string());
    }
    let parsed = Url::parse(repo_url)
        .map_err(|e| format!("仓库地址无法解析（仅支持完整 https:// URL）: {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "git 导入仅支持 https:// 仓库地址（收到 {}，ssh/file/ext 等协议一律拒绝）",
            parsed.scheme()
        ));
    }
    // 重建：只保留 scheme + host + port + path，userinfo/query/fragment 全部丢弃
    let mut clean = parsed.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    clean.set_query(None);
    clean.set_fragment(None);
    Ok(clean.to_string())
}

/// branch 参数校验：拒绝空值、空白、控制字符与选项形态（`-` 开头会被 git
/// 解释为选项，如 `--upload-pack=<cmd>`）。
pub fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty() {
        return Err("branch 不能为空".to_string());
    }
    if branch.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("branch 不能包含空白或控制字符".to_string());
    }
    if branch.starts_with('-') {
        return Err(format!(
            "branch 不能以 - 开头（会被 git 解释为选项）: {branch}"
        ));
    }
    Ok(())
}

/// 构造经 `git -c http.extraHeader=...` 注入的 Basic 凭证头。
/// 用户名固定 `x-access-token`（GitHub PAT 等的规范形态），token 只出现在
/// 该配置值里——不进 URL，git 的错误输出也不会回显它。
pub fn git_auth_header(token: &str) -> String {
    let raw = format!("x-access-token:{token}");
    format!(
        "Authorization: Basic {}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw)
    )
}

/// git clone 失败信息消毒：整行剥离任何包含 token（明文或 extraHeader 形态）的行，
/// 再截断到 500 字符。git 报错常回显完整 URL 与配置值，不消毒就会把凭证写进
/// kb_sources 状态字段与事件流。
pub fn sanitize_git_error(stderr: &str, token: Option<&str>) -> String {
    let mut text = stderr.to_string();
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        let header = git_auth_header(token);
        text = text
            .lines()
            .map(|line| {
                if line.contains(token) || line.contains(&header) {
                    "[已移除含凭证的行]"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    text.chars().take(500).collect::<String>()
}

// ─── local_dir ──────────────────────────────────────────────────────

/// 读取设置项 `kb.import.allowed_roots`（字符串数组）：数据目录之外允许导入的
/// 白名单根目录。留空 = 仅数据目录内可导入。
pub fn allowed_roots_from_settings(
    settings: &crate::settings_store::SettingsStore,
) -> Vec<PathBuf> {
    let Some(serde_json::Value::Array(items)) = settings.get("kb.import.allowed_roots") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 校验导入目录在允许范围内：canonicalize（解析符号链接/junction）后必须位于
/// 数据目录或任一白名单根之内。
///
/// 符号链接逃逸防御：链接在规范化中被解析，指向根外的路径其规范形态必然落在
/// 所有允许根之外 → 拒绝；目录不存在/不可访问同样拒绝（fail-closed）。
pub fn validate_local_dir(
    dir: &Path,
    data_dir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    let canonical = dir.canonicalize().map_err(|e| {
        format!(
            "导入目录无法访问: {} ({e})。默认仅允许数据目录内的路径；\
             如需导入其它目录，请在设置 kb.import.allowed_roots（字符串数组）中显式加入白名单根",
            dir.display()
        )
    })?;

    let mut roots: Vec<PathBuf> = vec![];
    if let Ok(root) = data_dir.canonicalize() {
        roots.push(root);
    }
    for extra in allowed_roots {
        match extra.canonicalize() {
            Ok(root) => roots.push(root),
            Err(_) => {
                return Err(format!(
                    "kb.import.allowed_roots 白名单根不存在或不可访问: {}",
                    extra.display()
                ))
            }
        }
    }

    for root in &roots {
        if canonical.starts_with(root) {
            return Ok(canonical);
        }
    }

    let allowed_list = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "导入目录 {} 不在允许范围内（允许: {allowed_list}）。\
         默认仅允许数据目录内的路径，外部目录需在设置 kb.import.allowed_roots 中显式加入白名单",
        canonical.display()
    ))
}

// ─── url（SSRF 防护） ───────────────────────────────────────────────

/// 导入 URL 解析与 scheme 校验：仅 http/https。
pub fn parse_import_url(url_str: &str) -> Result<Url, String> {
    let parsed = Url::parse(url_str.trim()).map_err(|e| format!("URL 无法解析: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!(
            "URL 导入仅支持 http/https（收到 {other}，file/ftp 等一律拒绝）"
        )),
    }
}

/// 判断 IP 是否为内网/保留目标（SSRF 防护的纯分类函数）。
/// 拒绝：环回、私网、链路本地（含云元数据 169.254.169.254）、唯一本地、
/// 未指定、广播、组播、文档段、IPv4 映射形态中的上述任意一类。
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || is_cgnat_v4(&v4)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                // ::ffff:a.b.c.d 按内嵌 IPv4 规则再判一次
                return is_forbidden_ip(IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local_v6(&v6)
                || is_link_local_v6(&v6)
        }
    }
}

/// IPv4 CGNAT 共享地址段（100.64.0.0/10）：标准库谓词未稳定，按位段判断。
fn is_cgnat_v4(v4: &std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (0x40..=0x7f).contains(&o[1])
}

/// IPv6 唯一本地地址（ULA，fc00::/7）：标准库没有谓词，按首字节位段判断。
fn is_unique_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

/// IPv6 链路本地（fe80::/10）：标准库谓词未稳定，按位段判断。
fn is_link_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    let o = v6.octets();
    o[0] == 0xfe && (o[1] & 0xc0) == 0x80
}

/// 对主机名做 DNS 解析并校验全部 A/AAAA 记录：任何一条命中内网/保留地址即拒绝；
/// 一条都解析不出也拒绝（fail-closed）。IP 字面量主机直接走同一分类。
pub async fn resolve_and_validate_host(host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
    use tokio::net::lookup_host;

    // 先试 IP 字面量（避免对 IP 做 DNS 解析的怪异行为）
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err(format!("目标地址 {ip} 属于内网/保留地址段，已拒绝"));
        }
        return Ok(vec![ip]);
    }

    let addrs: Vec<std::net::SocketAddr> = lookup_host((host, port))
        .await
        .map_err(|e| format!("主机名解析失败: {host} ({e})"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("主机名没有可用的解析记录: {host}"));
    }
    let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
    for ip in &ips {
        if is_forbidden_ip(*ip) {
            return Err(format!(
                "主机 {host} 的解析记录 {ip} 属于内网/保留地址段，已拒绝"
            ));
        }
    }
    Ok(ips)
}

/// 校验导入 URL 的目标主机（scheme + DNS 解析结果），返回解析后的 URL。
/// 每次重定向跳转前都要重新过这个校验。
pub async fn validate_import_url(url_str: &str) -> Result<Url, String> {
    let url = parse_import_url(url_str)?;
    let port = url.port_or_known_default().unwrap_or(80);
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL 缺少主机名: {url_str}"))?
        .to_string();
    resolve_and_validate_host(&host, port).await?;
    Ok(url)
}

/// 判断重定向是否允许跟随：仅同主机、端口不变（显式端口出现变化即拒绝）。
/// 跨主机重定向是 SSRF 探测的常用路径（先指向公网再 302 到内网/元数据端点）；
/// 同主机的 http/https scheme 切换不构成 SSRF（每跳仍会重新做 scheme + DNS 校验）。
/// `Location` 允许是相对引用，先基于当前 URL 解析再比较。
pub fn redirect_allowed(from: &Url, to: &str) -> Result<Url, String> {
    let target = from
        .join(to)
        .map_err(|e| format!("重定向地址无法解析（{to}）: {e}"))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(format!("重定向目标仅支持 http/https: {to}"));
    }
    if target.host_str() != from.host_str() {
        return Err(format!(
            "拒绝跟随跨主机重定向: {} -> {}",
            from.host_str().unwrap_or("?"),
            to
        ));
    }
    if target.port() != from.port() {
        return Err(format!("拒绝跟随改变端口的重定向: {to}"));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── git ──

    #[test]
    fn git_urls_must_be_https_without_userinfo() {
        assert_eq!(
            normalize_git_url("https://github.com/org/repo.git").unwrap(),
            "https://github.com/org/repo.git"
        );
        // 端口与大小写保留
        assert_eq!(
            normalize_git_url("https://git.example.com:8443/a/b.git").unwrap(),
            "https://git.example.com:8443/a/b.git"
        );
        // userinfo 丢弃（凭证只能走 extraHeader）
        assert_eq!(
            normalize_git_url("https://user:pass@git.example.com/a.git").unwrap(),
            "https://git.example.com/a.git"
        );
        // query/fragment 丢弃
        assert_eq!(
            normalize_git_url("https://github.com/a/b.git?x=1#frag").unwrap(),
            "https://github.com/a/b.git"
        );
        for bad in [
            "http://github.com/org/repo.git",        // 明文 http
            "ssh://git@github.com/org/repo.git",     // ssh
            "git@github.com:org/repo.git",           // scp 形态（非绝对 URL）
            "ext::sh -c id",                         // ext 伪协议 = 命令执行
            "file:///srv/git/repo.git",              // 本地路径
            "https://github.com/org/repo.git evil",  // 含空白
            "github.com/org/repo",                   // 无 scheme
            "https://github.com/org/repo.git\nHEAD", // 换行注入
        ] {
            assert!(normalize_git_url(bad).is_err(), "应拒绝: {bad:?}");
        }
    }

    #[test]
    fn branch_rejects_option_shapes_and_whitespace() {
        assert!(validate_branch("main").is_ok());
        assert!(validate_branch("release/1.2").is_ok());
        assert!(validate_branch("feat/x_y").is_ok());
        for bad in [
            "",
            "-",
            "--upload-pack=touch /tmp/pwn",
            " main",
            "main ",
            "ma in",
            "main\n--depth=1",
        ] {
            assert!(validate_branch(bad).is_err(), "branch 应被拒绝: {bad:?}");
        }
    }

    #[test]
    fn git_error_output_is_stripped_of_credentials() {
        let token = "ghp_secretToken1234567890";
        let stderr = "fatal: unable to access 'https://github.com/org/repo.git/': \
could not read Username for 'https://x-access-token:ghp_secretToken1234567890@github.com': \
terminal prompts disabled\nremote: Repository not found.";
        let sanitized = sanitize_git_error(stderr, Some(token));
        assert!(!sanitized.contains(token), "消毒后不得含明文 token");
        assert!(
            sanitized.contains("Repository not found"),
            "无关错误信息保留"
        );
        // 无 token 时仅截断
        let long = "x".repeat(2000);
        assert_eq!(sanitize_git_error(&long, None).chars().count(), 500);
    }

    // ── local_dir ──

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("waliapi-import-guard-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn local_dir_must_stay_inside_data_dir_or_whitelisted_roots() {
        let data_dir = temp_root("data");
        let inside = data_dir.join("docs");
        std::fs::create_dir_all(&inside).unwrap();

        // 数据目录内：放行
        assert!(validate_local_dir(&inside, &data_dir, &[]).is_ok());
        // 数据目录外（本机临时目录的其它路径）：拒绝
        let outside = temp_root("outside");
        assert!(validate_local_dir(&outside, &data_dir, &[]).is_err());
        // 加入白名单后：放行
        assert!(validate_local_dir(&outside, &data_dir, std::slice::from_ref(&outside)).is_ok());
        // 相对路径 / 不存在：拒绝（fail-closed）
        assert!(validate_local_dir(Path::new("relative/dir"), &data_dir, &[]).is_err());
        assert!(
            validate_local_dir(&data_dir.join("not-exist"), &data_dir, &[]).is_err(),
            "不存在的目录应被拒绝"
        );
        // 白名单根本身不存在：拒绝并提示
        assert!(
            validate_local_dir(&outside, &data_dir, &[PathBuf::from("/nonexistent-root")]).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_dir_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let data_dir = temp_root("sym");
        let outside = temp_root("sym-outside");
        let link = data_dir.join("escape");
        let _ = std::fs::remove_file(&link);
        symlink(&outside, &link).unwrap();
        // 指向数据目录外的符号链接：canonicalize 后落在根外 → 拒绝
        assert!(validate_local_dir(&link, &data_dir, &[]).is_err());
    }

    // ── url / SSRF ──

    #[test]
    fn import_urls_must_be_http_or_https() {
        assert!(parse_import_url("https://example.com/doc.md").is_ok());
        assert!(parse_import_url("http://example.com:8080/doc.md").is_ok());
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com",
            "not a url",
        ] {
            assert!(parse_import_url(bad).is_err(), "URL 应被拒绝: {bad}");
        }
    }

    #[test]
    fn forbidden_ip_classification() {
        let forbidden_v4 = [
            "127.0.0.1",       // 环回
            "127.8.8.8",       // 整个 127/8
            "10.0.0.1",        // 私网 10/8
            "172.16.0.1",      // 私网 172.16/12 起点
            "172.31.255.255",  // 私网 172.16/12 终点
            "192.168.1.1",     // 私网 192.168/16
            "169.254.169.254", // 链路本地（云元数据）
            "169.254.0.1",     // 链路本地
            "0.0.0.0",         // 未指定
            "255.255.255.255", // 广播
            "224.0.0.1",       // 组播
            "192.0.2.1",       // 文档段
            "100.64.0.1",      // CGNAT 共享段
        ];
        for ip in forbidden_v4 {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_forbidden_ip(ip), "{ip} 应为禁止地址");
        }
        let allowed_v4 = ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.32.0.1"];
        for ip in allowed_v4 {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_forbidden_ip(ip), "{ip} 应为允许地址");
        }

        let forbidden_v6 = [
            "::1",                    // 环回
            "::",                     // 未指定
            "fe80::1",                // 链路本地
            "fc00::1",                // ULA fc00::/7
            "fd12:3456:789a::1",      // ULA（fd 前缀）
            "ff02::1",                // 组播
            "::ffff:127.0.0.1",       // IPv4 映射环回
            "::ffff:169.254.169.254", // IPv4 映射链路本地
            "::ffff:10.1.2.3",        // IPv4 映射私网
        ];
        for ip in forbidden_v6 {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_forbidden_ip(ip), "{ip} 应为禁止地址");
        }
        // 公网 IPv6 放行（2001:db8::/32 文档段在 IPv6 侧无标准库谓词，按公网处理）
        let allowed_v6 = ["2606:4700:4700::1111", "2001:db8:4001::1"];
        for ip in allowed_v6 {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_forbidden_ip(ip), "{ip} 应为允许地址");
        }
    }

    #[tokio::test]
    async fn ip_literal_hosts_are_validated_without_dns() {
        assert!(resolve_and_validate_host("127.0.0.1", 80).await.is_err());
        assert!(resolve_and_validate_host("169.254.169.254", 80)
            .await
            .is_err());
        assert!(resolve_and_validate_host("::1", 80).await.is_err());
        // 公网 IP 字面量放行（不依赖 DNS，测试确定性）
        assert!(resolve_and_validate_host("8.8.8.8", 80).await.is_ok());
    }

    #[test]
    fn redirects_must_not_cross_hosts() {
        let from = Url::parse("https://example.com/a").unwrap();
        assert!(redirect_allowed(&from, "https://example.com/b").is_ok());
        assert!(redirect_allowed(&from, "/b").is_ok()); // 相对路径 = 同主机
        assert!(redirect_allowed(&from, "http://example.com/b").is_ok()); // 同主机换 scheme 仍同源端口
        assert!(redirect_allowed(&from, "https://evil.com/b").is_err());
        assert!(
            redirect_allowed(&from, "https://example.com:8443/b").is_err(),
            "端口变化视为跨主机"
        );
        assert!(redirect_allowed(&from, "http://127.0.0.1/x").is_err());
    }
}
