//! 业务命令：列表 / 详情 / 播放 / 评论（免费无登录纯净版）。

use crate::net::{base, edge};
use serde_json::{json, Value};
use tauri::State;
use tauri_plugin_opener::OpenerExt;

pub struct Api;

impl Api {
    pub fn new() -> Self {
        Self
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let origin = base();
        match edge(&origin, method, path, body.clone(), None).await {
            Ok(v) => Ok(v),
            Err(err) => {
                if let Some(next_origin) = crate::net::auto_failover(&origin).await {
                    if next_origin != origin {
                        return edge(&next_origin, method, path, body, None).await;
                    }
                }
                Err(err)
            }
        }
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        self.request("GET", path, None).await
    }

    /// POST（免费免登录调用）：新播放接口需要。
    async fn post(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        self.request("POST", path, body).await
    }
}

/// 过滤上游列表中的广告条目（如有）。
fn strip_ad_items(mut data: Value) -> Value {
    if let Some(items) = data.get_mut("items").and_then(Value::as_array_mut) {
        items.retain(|it| {
            let is_ad = it.get("is_ad").and_then(Value::as_bool).unwrap_or(false)
                || it.get("is_ad").and_then(Value::as_i64) == Some(1)
                || it.get("ad_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
            !is_ad
        });
    }
    data
}

/// 兼容新版评论字段：部分接口把正文放在 `content` 而非 `text`。
fn normalize_comments(mut data: Value) -> Value {
    if let Some(items) = data.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(obj) = item.as_object_mut() {
                let has_text = obj
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if !has_text {
                    if let Some(content) = obj.get("content").and_then(Value::as_str).map(str::to_owned) {
                        obj.insert("text".into(), json!(content));
                    }
                }
                let has_time = obj
                    .get("time")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if !has_time {
                    if let Some(created) = obj.get("created_at").and_then(Value::as_str).map(str::to_owned) {
                        obj.insert("time".into(), json!(created));
                    }
                }
            }
        }
    }
    data
}

/// API/运行错误与自检统一写入 debug.log，每行附 RFC 3339 UTC 时间和进程编号。
#[tauri::command]
pub async fn debug_log(app: tauri::AppHandle, msg: String) -> Result<(), String> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    crate::logging::append_debug(&dir, &msg)
}

/// 网络自检：系统代理 / DNS / 连通性快照，用于远程定位「打不开」。
#[tauri::command]
pub async fn net_diag() -> Result<Value, String> {
    Ok(crate::net::diagnose().await)
}

/* ───────────────────────── 线路（数据源） ───────────────────────── */

/// 当前线路 + 固定线路清单（镜像线路另外按需拉取）。
#[tauri::command]
pub async fn line_state() -> Result<Value, String> {
    Ok(json!({
        "base": base(),
        "default": crate::net::DEFAULT_BASE,
        "fixed": crate::lines::fixed(),
    }))
}

/// 手动切换线路：立即生效并落盘（下次启动沿用）。
#[tauri::command]
pub async fn line_set(url: String) -> Result<String, String> {
    crate::net::set_base(&url)
}

/// 从地址发布页抓镜像线路（只在用户点开「线路」时调用）。
#[tauri::command]
pub async fn line_mirrors() -> Result<Value, String> {
    crate::net::refresh_client();
    crate::lines::mirrors().await
}

/// 可达性探测：并发校验业务接口延迟。
#[tauri::command]
pub async fn line_probe(urls: Vec<String>) -> Result<Value, String> {
    crate::net::refresh_client();
    Ok(crate::lines::probe(urls).await)
}

/// 一键自动探测并切换到当前网络最快的可用线路。
#[tauri::command]
pub async fn line_auto_pick() -> Result<String, String> {
    crate::net::refresh_client();
    let best = crate::lines::find_fastest_line(None)
        .await
        .ok_or_else(|| "暂未探测到可用线路，请检查网络连接后重试".to_string())?;
    crate::net::set_base(&best)
}

/* ───────────────────────── 更新检查 ───────────────────────── */

const REPO: &str = "Henanshuaibi/HGhub";

fn ver_parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .map(|s| s.trim().parse::<u64>().unwrap_or(0))
        .collect()
}

/// 只比较数字段：`v1.2.0` > `1.1.9`；解析不出的段按 0 处理。
fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (ver_parts(latest), ver_parts(current));
    for i in 0..a.len().max(b.len()) {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 对比 GitHub Release 的 tag 与当前版本（未认证 60 次/小时/IP，够用）。
#[tauri::command]
pub async fn check_update() -> Result<Value, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = crate::net::get(
        &url,
        std::time::Duration::from_secs(12),
        Some("application/vnd.github+json"),
        None,
    )
    .await?;

    let status = resp.status();
    if status.as_u16() == 404 {
        // 还没发过 release
        return Ok(json!({
            "current": current, "latest": "", "has_update": false,
            "url": format!("https://github.com/{REPO}/releases"),
        }));
    }
    if !status.is_success() {
        return Err(format!("GitHub 返回 HTTP {status}"));
    }

    let v: Value = resp.json().await.map_err(|e| crate::net::err_text(&e))?;
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let html = v
        .get("html_url")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases"));
    let notes = v.get("body").and_then(|x| x.as_str()).unwrap_or("");

    Ok(json!({
        "current": current,
        "latest": tag.trim_start_matches(['v', 'V']).to_string(),
        "tag": tag,
        "has_update": !tag.is_empty() && is_newer(&tag, &current),
        "url": html,
        "name": v.get("name").and_then(|x| x.as_str()).unwrap_or(""),
        "published_at": v.get("published_at").and_then(|x| x.as_str()).unwrap_or(""),
        "notes": notes.chars().take(800).collect::<String>(),
    }))
}

/// 用系统默认浏览器打开链接（只允许 http/https）。
#[tauri::command]
pub async fn open_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let target = crate::network_policy::parse_target(&url)?;
    // 固定使用默认处理器；不向前端暴露可指定程序的 opener 命令。
    app.opener()
        .open_url(target.as_str(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// 切换移动端屏幕方向与沉浸系统栏（iOS / Android 原生旋转屏幕方向：横屏 Landscape / 竖屏 Portrait）。
#[tauri::command]
pub async fn set_orientation(
    app: tauri::AppHandle,
    landscape: bool,
    immersive: Option<bool>,
) -> Result<(), String> {
    #[cfg(target_os = "ios")]
    {
        let _ = immersive;
        let _ = app.run_on_main_thread(move || {
            request_ios_orientation(landscape);
        });
    }
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        request_android_orientation(landscape, immersive.unwrap_or(landscape));
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        let _ = (&app, landscape, immersive);
    }
    Ok(())
}

/// 读取 Android Edge-to-Edge 安全区高度（状态栏/挖孔屏 top 与导航栏 bottom，单位 dp）。
#[tauri::command]
pub async fn get_safe_insets() -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let (top, bottom) = read_android_safe_insets();
        Ok(json!({ "top": top, "bottom": bottom }))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(json!({ "top": 0, "bottom": 0 }))
    }
}

/// 同步 Android 状态栏与导航栏前景图标深浅（浅色内容页用深色图标，深色/刷剧页用亮色图标）。
#[tauri::command]
pub async fn set_status_bar_light(light: bool) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        request_android_status_bar_light(light);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = light;
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn with_android_activity<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>) -> Option<R>,
{
    let ctx = ndk_context::android_context();
    let vm_ptr = ctx.vm();
    let activity_ptr = ctx.context();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        return None;
    }
    unsafe {
        let vm = std::mem::ManuallyDrop::new(jni::JavaVM::from_raw(vm_ptr.cast()).ok()?);
        let mut env = vm.attach_current_thread_permanently().ok()?;
        let activity =
            std::mem::ManuallyDrop::new(jni::objects::JObject::from_raw(activity_ptr.cast()));
        let out = f(&mut env, &activity);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        out
    }
}

#[cfg(target_os = "android")]
fn request_android_orientation(landscape: bool, immersive: bool) {
    let _ = with_android_activity(|env, activity| {
        let res = env.call_method(
            activity,
            "applyImmersiveOrientation",
            "(ZZ)V",
            &[
                jni::objects::JValue::Bool(u8::from(landscape)),
                jni::objects::JValue::Bool(u8::from(immersive)),
            ],
        );
        if res.is_err() {
            let _ = env.exception_clear();
            // 6 = SCREEN_ORIENTATION_SENSOR_LANDSCAPE, -1 = SCREEN_ORIENTATION_UNSPECIFIED
            let mode: i32 = if landscape { 6 } else { -1 };
            let _ = env.call_method(
                activity,
                "setRequestedOrientation",
                "(I)V",
                &[jni::objects::JValue::Int(mode)],
            );
            let _ = env.exception_clear();
        }
        Some(())
    });
}

#[cfg(target_os = "android")]
fn read_android_safe_insets() -> (u32, u32) {
    with_android_activity(|env, activity| {
        let packed = env
            .call_method(activity, "getSafeInsetsPacked", "()J", &[])
            .and_then(|v| v.j())
            .ok()?;
        let top = ((packed >> 16) & 0xFFFF) as u32;
        let bottom = (packed & 0xFFFF) as u32;
        Some((top, bottom))
    })
    .unwrap_or((0, 0))
}

#[cfg(target_os = "android")]
fn request_android_status_bar_light(light: bool) {
    let _ = with_android_activity(|env, activity| {
        let _ = env.call_method(
            activity,
            "applyStatusBarLight",
            "(Z)V",
            &[jni::objects::JValue::Bool(u8::from(light))],
        );
        let _ = env.exception_clear();
        Some(())
    });
}

#[cfg(target_os = "ios")]
fn request_ios_orientation(landscape: bool) {
    use std::ffi::{c_char, c_void};
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    unsafe {
        let msg0: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let msg0_usize: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let msg1_usize: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let msg2_ptr: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        let msg1_void: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());

        // 1. iOS 16+ UIWindowScene.requestGeometryUpdateWithPreferences:errorHandler:
        // UIInterfaceOrientationMaskPortrait = 1 << 1 = 2
        // UIInterfaceOrientationMaskLandscapeRight = 1 << 3 = 8
        let mask: usize = if landscape { 8 } else { 2 };
        let prefs_cls = objc_getClass(b"UIWindowSceneGeometryPreferencesIOS\0".as_ptr() as *const c_char);
        let app_cls = objc_getClass(b"UIApplication\0".as_ptr() as *const c_char);
        if !prefs_cls.is_null() && !app_cls.is_null() {
            let sel_alloc = sel_registerName(b"alloc\0".as_ptr() as *const c_char);
            let sel_init = sel_registerName(b"initWithInterfaceOrientations:\0".as_ptr() as *const c_char);
            let raw_prefs = msg0(prefs_cls, sel_alloc);
            let prefs = if !raw_prefs.is_null() {
                msg1_usize(raw_prefs, sel_init, mask)
            } else {
                std::ptr::null_mut()
            };

            let sel_shared = sel_registerName(b"sharedApplication\0".as_ptr() as *const c_char);
            let app = msg0(app_cls, sel_shared);
            if !app.is_null() && !prefs.is_null() {
                let sel_scenes = sel_registerName(b"connectedScenes\0".as_ptr() as *const c_char);
                let scenes_set = msg0(app, sel_scenes);
                if !scenes_set.is_null() {
                    let sel_all = sel_registerName(b"allObjects\0".as_ptr() as *const c_char);
                    let arr = msg0(scenes_set, sel_all);
                    if !arr.is_null() {
                        let sel_count = sel_registerName(b"count\0".as_ptr() as *const c_char);
                        let sel_at = sel_registerName(b"objectAtIndex:\0".as_ptr() as *const c_char);
                        let sel_req = sel_registerName(
                            b"requestGeometryUpdateWithPreferences:errorHandler:\0".as_ptr() as *const c_char,
                        );
                        let sel_responds = sel_registerName(b"respondsToSelector:\0".as_ptr() as *const c_char);
                        let msg_responds: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool =
                            std::mem::transmute(objc_msgSend as *const ());
                        let count = msg0_usize(arr, sel_count);
                        for i in 0..count {
                            let scene = msg1_usize(arr, sel_at, i);
                            if !scene.is_null() && msg_responds(scene, sel_responds, sel_req) {
                                msg2_ptr(scene, sel_req, prefs, std::ptr::null_mut());
                            }
                        }
                    }
                }
            }
        }

        // 2. Fallback: UIDevice setValue:forKey:@"orientation" + attemptRotationToDeviceOrientation
        let dev_cls = objc_getClass(b"UIDevice\0".as_ptr() as *const c_char);
        let num_cls = objc_getClass(b"NSNumber\0".as_ptr() as *const c_char);
        let str_cls = objc_getClass(b"NSString\0".as_ptr() as *const c_char);
        let vc_cls = objc_getClass(b"UIViewController\0".as_ptr() as *const c_char);
        if !dev_cls.is_null() && !num_cls.is_null() && !str_cls.is_null() {
            let sel_cur = sel_registerName(b"currentDevice\0".as_ptr() as *const c_char);
            let dev = msg0(dev_cls, sel_cur);
            let sel_num = sel_registerName(b"numberWithInteger:\0".as_ptr() as *const c_char);
            let orient_val: usize = if landscape { 3 } else { 1 };
            let num = msg1_usize(num_cls, sel_num, orient_val);
            let sel_str = sel_registerName(b"stringWithUTF8String:\0".as_ptr() as *const c_char);
            let msg_str: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let key = msg_str(str_cls, sel_str, b"orientation\0".as_ptr() as *const c_char);
            if !dev.is_null() && !num.is_null() && !key.is_null() {
                let sel_set_val = sel_registerName(b"setValue:forKey:\0".as_ptr() as *const c_char);
                msg2_ptr(dev, sel_set_val, num, key);
            }
            if !vc_cls.is_null() {
                let sel_attempt = sel_registerName(b"attemptRotationToDeviceOrientation\0".as_ptr() as *const c_char);
                msg1_void(vc_cls, sel_attempt);
            }
        }
    }
}

/* ───────────────────────── 启动初始化 ───────────────────────── */

#[tauri::command]
pub async fn app_boot() -> Result<Value, String> {
    let origin = base();
    Ok(json!({
        "user": Value::Null,
        "logged_in": false,
        "base": origin,
        "strict_security": crate::network_policy::strict_security(),
        "cli_relaxed": crate::network_policy::cli_strict_override() == Some(false),
    }))
}

/// 切换严格安全模式（关闭后回退到兼容清洗与网络守卫模式，立即生效并落盘）。
#[tauri::command]
pub async fn security_set(strict: bool) -> Result<bool, String> {
    crate::network_policy::set_strict_security(strict)?;
    crate::net::refresh_client();
    crate::playback_log::record(
        "security.mode_changed",
        json!({"strict": crate::network_policy::strict_security()}),
    );
    Ok(crate::network_policy::strict_security())
}

/* ───────────────────────── 列表 ───────────────────────── */

#[tauri::command]
pub async fn api_feed(state: State<'_, Api>, page: u32, size: u32) -> Result<Value, String> {
    state
        .get(&format!("/api/videos?page={page}&size={size}"))
        .await
        .map(strip_ad_items)
}

#[tauri::command]
pub async fn api_category(
    state: State<'_, Api>,
    cate: String,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!(
            "/api/videos/category/{cate}?page={page}&size={size}"
        ))
        .await
        .map(strip_ad_items)
}

#[tauri::command]
pub async fn api_rank(state: State<'_, Api>, kind: String) -> Result<Value, String> {
    let kind = match kind.as_str() {
        "recommend" | "potential" => kind,
        _ => "hot".to_string(),
    };
    state.get(&format!("/api/ranks/{kind}")).await
}

#[tauri::command]
pub async fn api_search(
    state: State<'_, Api>,
    q: String,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    state
        .get(&format!("/api/search?q={encoded}&page={page}&size={size}"))
        .await
        .map(strip_ad_items)
}

#[tauri::command]
pub async fn api_posts(state: State<'_, Api>, page: u32, size: u32) -> Result<Value, String> {
    state
        .get(&format!("/api/posts?page={page}&size={size}"))
        .await
        .map(strip_ad_items)
}

#[tauri::command]
pub async fn api_post(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/posts/{id}")).await
}

/* ───────────────────────── 详情 / 播放 ───────────────────────── */

#[tauri::command]
pub async fn api_video(state: State<'_, Api>, id: i64) -> Result<Value, String> {
    state.get(&format!("/api/videos/{id}")).await
}

/// 新版播放接口返回 `streams.{main,hevc,preview}` 与 `cdn.{authKey,authValue}`。
/// 这里回填 `video_url` / `video_url_hevc` 并同步 CDN 鉴权头供媒体代理透传。
fn normalize_play_info(info: &mut Value) {
    fn non_empty(v: Option<&Value>) -> Option<String> {
        v.and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
    if let Some(cdn) = info.get("cdn") {
        let key = non_empty(cdn.get("authKey"));
        let val = non_empty(cdn.get("authValue"));
        crate::media::set_cdn_auth(key, val);
    }
    let main = non_empty(info.get("streams").and_then(|s| s.get("main")));
    let hevc = non_empty(info.get("streams").and_then(|s| s.get("hevc")));
    let Some(obj) = info.as_object_mut() else { return };
    for (field, stream) in [("video_url", main), ("video_url_hevc", hevc)] {
        if let Some(url) = stream {
            if non_empty(obj.get(field)).is_none() {
                obj.insert(field.into(), json!(url));
            }
        }
    }
}

fn has_play_url(info: &Value) -> bool {
    info.get("video_url")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

#[tauri::command]
pub async fn api_play(state: State<'_, Api>, id: i64, ep: u32) -> Result<Value, String> {
    // 优先走新接口；失败或没有可用地址时再退回旧接口，两者都不行就返回新接口的结果/错误。
    let primary = state
        .post(&format!("/api/videos/{id}/episodes/{ep}/playback"), Some(json!({})))
        .await
        .map(|mut info| {
            normalize_play_info(&mut info);
            info
        });
    if let Ok(info) = &primary {
        if has_play_url(info) {
            return primary;
        }
    }
    match state
        .get(&format!("/api/videos/{id}/play?ep={ep}&page_texts=0"))
        .await
    {
        Ok(mut legacy) => {
            normalize_play_info(&mut legacy);
            if has_play_url(&legacy) || primary.is_err() {
                Ok(legacy)
            } else {
                primary
            }
        }
        Err(_) => primary,
    }
}

/* ───────────────────────── 评论（只读，免登录） ───────────────────────── */

#[tauri::command]
pub async fn api_comments(
    state: State<'_, Api>,
    id: i64,
    ep: u32,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!(
            "/api/videos/{id}/comments?ep={ep}&page={page}&size={size}"
        ))
        .await
        .map(normalize_comments)
}

/* ───────────────────────── 本地缓存 ───────────────────────── */

#[tauri::command]
pub async fn cache_clear() -> Result<(), String> {
    crate::media::clear_caches();
    Ok(())
}

/* ───────────────────────── 吃瓜帖子评论 ───────────────────────── */

#[tauri::command]
pub async fn api_post_comments(
    state: State<'_, Api>,
    id: i64,
    page: u32,
    size: u32,
) -> Result<Value, String> {
    state
        .get(&format!("/api/posts/{id}/comments?page={page}&size={size}"))
        .await
        .map(normalize_comments)
}
#[cfg(test)]
mod tests {
    use super::{has_play_url, is_newer, normalize_play_info};
    use serde_json::{json, Value};

    #[test]
    fn new_playback_streams_fill_legacy_url_fields() {
        let mut info = json!({
            "video_url": "",
            "streams": {"main": "https://x/main.m3u8", "hevc": "https://x/hevc.m3u8", "preview": "https://x/p.m3u8"}
        });
        normalize_play_info(&mut info);
        assert_eq!(info["video_url"], "https://x/main.m3u8");
        assert_eq!(info["video_url_hevc"], "https://x/hevc.m3u8");
        assert!(has_play_url(&info));
    }

    #[test]
    fn existing_video_url_is_kept_and_empty_streams_stay_empty() {
        let mut keep = json!({"video_url": "https://old/a.m3u8", "streams": {"main": "https://x/main.m3u8"}});
        normalize_play_info(&mut keep);
        assert_eq!(keep["video_url"], "https://old/a.m3u8");

        let mut empty = json!({"video_url": "", "streams": {"main": ""}});
        normalize_play_info(&mut empty);
        assert!(!has_play_url(&empty));
        assert!(!has_play_url(&json!({"id": 1})));
    }

    #[test]
    fn frontend_cannot_bypass_url_policy_with_opener_commands() {
        let capability: Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        for permission in capability["permissions"].as_array().unwrap() {
            let name = permission
                .as_str()
                .or_else(|| permission["identifier"].as_str())
                .unwrap();
            assert!(
                !name.starts_with("opener:"),
                "frontend opener command exposed: {name}"
            );
        }
    }

    #[test]
    fn external_links_are_urls_not_shell_commands() {
        for raw in [
            "https://example.com/|calc.exe",
            "https://example.com/?a=1&b=2",
            "https://example.com/%25PATH%25",
            "https://example.com/%VAR%",
        ] {
            assert!(crate::network_policy::parse_target(raw).is_ok());
        }
        for raw in [
            "file:///etc/passwd",
            "mailto:user@example.com",
            "tel:123",
            "data:text/html,test",
            "javascript:alert(1)",
            "https://example.com/\0cmd",
            "https://user:password@example.com/",
        ] {
            assert!(crate::network_policy::parse_target(raw).is_err());
        }
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v1.0.2", "1.0.1"));
        assert!(is_newer("v1.1", "1.0.9"));
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(!is_newer("v1.0.1", "1.0.1"));
        assert!(!is_newer("v0.9.9", "1.0.1"));
        assert!(!is_newer("", "1.0.1"));
    }
}
