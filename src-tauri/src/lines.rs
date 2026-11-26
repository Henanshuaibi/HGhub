//! 线路（数据源）：固定线路 + 地址发布页上的镜像线路。
//!
//! 上游只是域名在变，请求链路与数据模型完全一致，所以「切线路」等价于换 `net::base()`。
//! 镜像地址来自 <https://huangguoai.pages.dev/>：那个页面本身是空壳，真正的清单在
//! `publish.js` 里（`var urls` 域名数组 + `var subdomains` 子域数组，页面自己随机拼）。

use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// 固定线路：内置直连镜像与主站，确保国内外及开关 VPN 场景均可开箱直连。
pub const FIXED: [(&str, &str); 4] = [
    ("直连镜像 1（推荐免梯）", "https://thu.wsaditmwl.cc"),
    ("直连镜像 2（免梯备用）", "https://pku.xxslfswu.cc"),
    ("直连镜像 3（免梯备用）", "https://fdu.wsaditmwl.cc"),
    ("官方主站（海外/代理）", "https://huangguoai.com"),
];

const PUBLISH_PAGES: [&str; 2] = [
    "https://huangguoai.pages.dev/",
    "https://huang-guo.pages.dev/",
];
/// 镜像列表上限（子域 × 域名是笛卡尔积，界面只展示这么多）
const MIRROR_MAX: usize = 60;
/// 单条线路探测超时
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

pub fn fixed() -> Value {
    json!(FIXED
        .iter()
        .map(|(name, url)| json!({ "name": name, "url": url }))
        .collect::<Vec<_>>())
}

/// 拉地址发布页 → 解析 publish.js → 拼出镜像地址。
pub async fn mirrors() -> Result<Value, String> {
    let mut last_err = String::from("地址发布页不可达");
    for publish_page in PUBLISH_PAGES {
        match mirrors_from(publish_page).await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn mirrors_from(publish_page: &str) -> Result<Value, String> {
    let html = get_text(publish_page).await?;
    let js_rel = script_src(&html).unwrap_or_else(|| "publish.js".to_string());
    let js_url = absolute(publish_page, &js_rel);
    let js = get_text(&js_url).await?;

    let domains = js_array(&js, "var urls");
    let subs = js_array(&js, "var subdomains");
    if domains.is_empty() {
        return Err("地址发布页格式有变化：没找到镜像域名".to_string());
    }

    let mut items: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    'outer: for domain in domains {
        let domain = domain.trim_end_matches('/').to_string();
        if domain.is_empty() {
            continue;
        }
        let hosts: Vec<String> = if subs.is_empty() {
            vec![domain.clone()]
        } else {
            subs.iter().map(|s| format!("{s}.{domain}")).collect()
        };
        for host in hosts {
            if !seen.insert(host.clone()) {
                continue;
            }
            items.push(json!({ "url": format!("https://{host}"), "host": host }));
            if items.len() >= MIRROR_MAX {
                break 'outer;
            }
        }
    }
    if items.is_empty() {
        return Err("地址发布页里没有可用的镜像地址".to_string());
    }

    Ok(json!({ "source": publish_page, "script": js_url, "items": items }))
}

/// 真实探测某条线路的业务接口是否可用（校验 /api/videos 返回有效 JSON）。
pub async fn probe_api_ok(origin: &str) -> Result<u64, String> {
    let target = format!("{}/api/videos?page=1&size=1", origin.trim_end_matches('/'));
    let referer = format!("{}/", origin.trim_end_matches('/'));
    let started = Instant::now();
    let resp = crate::net::get(
        &target,
        PROBE_TIMEOUT,
        Some("application/json"),
        Some(&referer),
    )
    .await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| crate::net::err_text(&e))?;
    let val: Value = serde_json::from_str(&text).map_err(|_| "返回非 JSON".to_string())?;
    if val.get("status").and_then(Value::as_i64) == Some(1) {
        Ok(started.elapsed().as_millis() as u64)
    } else {
        Err("业务接口不可用".to_string())
    }
}

/// 自动并发探测内置线路与最新镜像，返回最快可用线路。
pub async fn find_fastest_line(exclude: Option<&str>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(ex) = exclude {
        seen.insert(ex.trim_end_matches('/').to_string());
    }
    for (_, u) in FIXED {
        let u = u.trim_end_matches('/').to_string();
        if seen.insert(u.clone()) {
            candidates.push(u);
        }
    }
    // 第一轮先并发测内置线路（最快，不额外等 publish.js）
    if let Some(best) = race_candidates(&candidates).await {
        return Some(best);
    }
    // 第二轮拉取地址发布页动态镜像
    if let Ok(m) = mirrors().await {
        let mut dynamic: Vec<String> = Vec::new();
        if let Some(arr) = m.get("items").and_then(Value::as_array) {
            for item in arr {
                if let Some(u) = item.get("url").and_then(Value::as_str) {
                    let u = u.trim_end_matches('/').to_string();
                    if seen.insert(u.clone()) {
                        dynamic.push(u);
                        if dynamic.len() >= 10 {
                            break;
                        }
                    }
                }
            }
        }
        if let Some(best) = race_candidates(&dynamic).await {
            return Some(best);
        }
    }
    None
}

async fn race_candidates(urls: &[String]) -> Option<String> {
    if urls.is_empty() {
        return None;
    }
    let mut handles = Vec::new();
    for u in urls {
        let u = u.clone();
        handles.push(tokio::spawn(async move {
            probe_api_ok(&u).await.map(|ms| (u, ms))
        }));
    }
    let mut winners: Vec<(String, u64)> = Vec::new();
    for h in handles {
        if let Ok(Ok(pair)) = h.await {
            winners.push(pair);
        }
    }
    winners.sort_by_key(|(_, ms)| *ms);
    winners.into_iter().next().map(|(u, _)| u)
}

/// 可达性探测（并发 6 条一组）。只报结果，不做自动切换。
pub async fn probe(urls: Vec<String>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for chunk in urls.chunks(6) {
        let mut handles = Vec::new();
        for u in chunk {
            let u = u.clone();
            handles.push(tokio::spawn(async move { probe_one(u).await }));
        }
        for h in handles {
            if let Ok(v) = h.await {
                out.push(v);
            }
        }
    }
    json!(out)
}

async fn probe_one(url: String) -> Value {
    let started = Instant::now();
    match probe_api_ok(&url).await {
        Ok(ms) => json!({ "url": url, "ok": true, "status": 200, "ms": ms }),
        Err(e) => {
            let ms = started.elapsed().as_millis() as u64;
            let status = e
                .strip_prefix("HTTP ")
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<u16>().ok());
            json!({ "url": url, "ok": false, "status": status, "ms": ms, "error": e })
        }
    }
}

async fn get_text(url: &str) -> Result<String, String> {
    let resp = crate::net::get(url, Duration::from_secs(12), None, None).await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.text().await.map_err(|e| crate::net::err_text(&e))
}

/// 从 HTML 里挑出脚本地址（发布页只有一个 `publish.js?<版本>`）。
fn script_src(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(i) = rest.find("src=\"") {
        let after = &rest[i + 5..];
        let end = after.find('"')?;
        let candidate = &after[..end];
        if candidate.contains(".js") {
            return Some(candidate.to_string());
        }
        rest = &after[end..];
    }
    None
}

fn absolute(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let origin = url::Url::parse(base)
        .ok()
        .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default()));
    match origin {
        Some(o) if rel.starts_with('/') => format!("{o}{rel}"),
        Some(o) => format!("{o}/{rel}"),
        None => rel.to_string(),
    }
}

/// 取 JS 里 `var <name> = [ ... ]` 的字符串数组（去掉行尾 `//` 注释，但保留字符串里的 `//`）。
fn js_array(src: &str, decl: &str) -> Vec<String> {
    let Some(at) = src.find(decl) else {
        return Vec::new();
    };
    let rest = &src[at..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest[open..].find(']') else {
        return Vec::new();
    };
    strip_line_comments(&rest[open + 1..open + close])
        .split(',')
        .map(|raw| {
            raw.trim()
                .trim_matches(|c: char| c == '\'' || c == '"')
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// 去掉 `//` 行尾注释（引号内的 `//` 是 URL，不能动）。
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut quote: Option<char> = None;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    out.push(c);
                } else if c == '/' && chars.peek() == Some(&'/') {
                    for n in chars.by_ref() {
                        if n == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
var subdomains = [
    'thu',  // 清华
    'pku',  // 北大
];
var urls=[
	'xxbgwypx.cc/',
];
var otherUrls = [
	'https://huangguoai.pages.dev',
];
var foreverUrls = [
	  'https://huangguoai.com',
	JumpPage,
	'https://huangguoai.ai',
];
"#;

    #[test]
    fn parses_publish_js_arrays() {
        assert_eq!(js_array(SAMPLE, "var urls"), vec!["xxbgwypx.cc/"]);
        assert_eq!(js_array(SAMPLE, "var subdomains"), vec!["thu", "pku"]);
        assert_eq!(
            js_array(SAMPLE, "var foreverUrls"),
            vec![
                "https://huangguoai.com",
                "JumpPage",
                "https://huangguoai.ai"
            ]
        );
    }

    #[test]
    fn resolves_relative_script() {
        assert_eq!(
            absolute("https://huangguoai.pages.dev/", "publish.js?20261103-2"),
            "https://huangguoai.pages.dev/publish.js?20261103-2"
        );
        assert_eq!(
            absolute("https://huangguoai.pages.dev/", "/a/b.js"),
            "https://huangguoai.pages.dev/a/b.js"
        );
    }

    #[test]
    fn picks_script_src() {
        let html = r#"<link rel="stylesheet" href="publish.css?1" /><script type="text/javascript" src="publish.js?20261103-2"></script>"#;
        assert_eq!(script_src(html).as_deref(), Some("publish.js?20261103-2"));
    }

    /// 手动跑：`cargo test --lib -- --ignored lines::tests::mirrors_live -- --nocapture`
    #[tokio::test]
    #[ignore = "联网测试：请求上游镜像列表，手动用 --ignored 运行"]
    async fn mirrors_live() {
        match mirrors().await {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).expect("json")),
            Err(e) => panic!("mirrors failed: {e}"),
        }
    }

    /// 手动跑：`cargo test --lib -- --ignored lines::tests::probe_live -- --nocapture`
    #[tokio::test]
    #[ignore = "联网测试：探测上游线路连通性，手动用 --ignored 运行"]
    async fn probe_live() {
        let urls = vec![
            "https://huangguoai.com".to_string(),
            "https://huangguoai.ai".to_string(),
            "https://thu.xxbgwypx.cc".to_string(),
        ];
        println!(
            "{}",
            serde_json::to_string_pretty(&probe(urls).await).expect("json")
        );
    }
}
