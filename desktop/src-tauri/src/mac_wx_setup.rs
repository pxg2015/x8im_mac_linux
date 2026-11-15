// 邦信 · macOS 读微信消息前置条件 + 提权拉起（2026-11-10）
// ─────────────────────────────────────────────────────────────────────────────
// macOS 上读微信消息比 Windows 多三道关，本模块集中处理：
//
//  1) 完全磁盘访问（FDA）：系统隐私设置里授予，才能读到微信沙盒容器里的 DB。
//  2) Hardened Runtime（HR）：微信默认带 HR，会拦 task_for_pid，wxgo 无法 attach。
//     必须「重签」微信——抽原 entitlements、去掉 runtime 标志、用本机 ad-hoc 证书
//     重签（保留 allow-jit / debugger，否则微信起不来）。重签要管理员密码。
//  3) 用户态拉起：微信以当前登录用户运行，macOS 的 TCC/SIP 限制下，root 进程
//     **无法 lldb-attach 到用户态微信**（密钥提取需在微信进程内断点捕获 passphrase）。
//     故 wxgo 必须以「当前用户」经 LaunchAgent 拉起，与微信同用户、同会话，lldb 才能挂上。
//     同事在 mac 上用「用户态 LaunchAgent + lldb 挂用户微信」已验证成功，本机对齐该路径。
//     免管理员密码（LaunchAgent 在 gui/$UID 域，普通用户即可 load）。
//
// ⚠️ 令牌只经 0600 临时文件（X8TOKEN_FILE 指向它）注入，绝不进 argv / plist 明文 /
//   环境变量值；wxgo 读后即删，落盘窗口极短。邦信本进程不持 wxgo 子进程句柄
//   （用户态进程），所以 plugin_host 在 mac 上靠端口探活。
//   升级兼容：从 1.6.83（root LaunchDaemon）升级时，旧 root 守护仍占端口，故首次
//   以管理员密码 bootout 旧 system 域守护（仅当 /Library/LaunchDaemons 下 plist
//   仍存在才弹窗，之后即不再弹）。

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

// 与 plugin_host.rs 同口径的启动等待常量（mac 经 LaunchAgent 拉起后等就绪）。
// 2026-11-12：从 12s 抬到 30s —— 慢启动（抢单实例锁 / 争端口 / 大库初始化）
// 不该被当成「启动失败」而回退旧版；判据同时改为以 ready 标记为主。
const BOOT_TIMEOUT_MS: u64 = 30000;
const PROBE_INTERVAL_MS: u64 = 100;

/// 微信主程序路径（标准安装位置）
fn wechat_app() -> String {
    "/Applications/WeChat.app".to_string()
}

/// 微信容器数据目录（用于 FDA 探活）：macOS 把第三方 app 数据放进沙盒容器，
/// 跨 app 读它通常需要「完全磁盘访问」权限。
fn wechat_container() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .map(|h| format!("{h}/Library/Containers/com.tencent.xinWeChat/Data"))
}

/// 微信是否已重签到「可被调试」的状态（两个条件缺一不可）：
///  1) 不带 Hardened Runtime —— `codesign -dvvv` 输出里没有 `Flags=0x10000[runtime]`；
///  2) 带 `com.apple.security.get-task-allow` —— 否则 taskgated 会拒掉 lldb 的
///     task_for_pid（实测：Developer mode disabled 时，非 root 与 root 一并被拒，
///     报 "Not allowed to attach to process"）。补上这个调试授权位才能免开开发者模式。
pub fn wechat_resigned() -> bool {
    wechat_resigned_at(&wechat_app())
}

/// 指定路径的 app 是否已重签到「可被调试」状态（备份校验也用它）。
pub fn wechat_resigned_at(app: &str) -> bool {
    let out = Command::new("/usr/bin/codesign")
        .args(["-dvvv", app])
        .output();
    let no_runtime = match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            // 含 "runtime" 即仍带 Hardened Runtime → 未重签
            !format!("{s}{e}").contains("runtime")
        }
        Err(_) => return false,
    };
    if !no_runtime {
        return false;
    }
    has_get_task_allow(app)
}

/// 目标 app 的 entitlements 里是否已带调试授权位（get-task-allow）。
fn has_get_task_allow(app: &str) -> bool {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-", app])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("get-task-allow"),
        Err(_) => false,
    }
}

/// 往 entitlements plist 里补 `com.apple.security.get-task-allow`（已存在则原样返回）。
fn ensure_get_task_allow(xml: &str) -> String {
    if xml.contains("get-task-allow") {
        return xml.to_string();
    }
    // 插到最后一个 </dict> 之前（根 dict 收尾处）
    match xml.rfind("</dict>") {
        Some(i) => {
            let mut s = String::with_capacity(xml.len() + 80);
            s.push_str(&xml[..i]);
            s.push_str("\t<key>com.apple.security.get-task-allow</key>\n\t<true/>\n");
            s.push_str(&xml[i..]);
            s
        }
        None => xml.to_string(),
    }
}

/// 微信是否正在运行（pgrep 匹配微信主进程）。
pub fn wechat_running() -> bool {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-f", "WeChat.app/Contents/MacOS/WeChat"])
        .output();
    match out {
        Ok(o) => o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// 完全磁盘访问（FDA）是否授予。此为「尽力探测」：尝试读取微信容器目录的元数据，
/// 读不到（权限拒绝）即视为未授予。真实可用性最终由 wxgo 报错兜底。
pub fn fda_granted() -> bool {
    match wechat_container() {
        Some(p) => Path::new(&p).join("Documents").metadata().is_ok(),
        None => false,
    }
}

/// 「App 管理（App Management，macOS 13+）」是否已授予邦信。
///
/// 为什么单列一个状态：重签微信要改 /Applications/WeChat.app 里的二进制，这被 TCC 的
/// 「App 管理」保护——FDA 管不到它。缺它时 `codesign --force` 会报
/// `Operation not permitted in subcomponent`，用户根本看不懂，只会反复点重签。
///
/// 探测方式与 resign_wechat 里的预检同款：往微信 App 里 touch 一个探针文件，
/// 能写入=有 App 管理权限（写完立即删，不留痕迹）。TCC 是进程级校验，只在进程启动时
/// 读取，故「授权后未 ⌘Q 重启」仍会探测失败——这恰好和提示语「勾选后必须重启邦信」对齐。
pub fn app_mgmt_granted() -> bool {
    let probe = format!("{}/Contents/.x8w_permchk", wechat_app());
    // 用 shell 串执行 touch + rm，避免留下探针文件；touch 成功即有权，rm 兜底清理。
    let ok = std::process::Command::new("/usr/bin/touch")
        .arg(&probe)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        let _ = std::fs::remove_file(&probe);
    }
    ok
}

/// macOS 13+ 的「App 管理（App Management）」/ 完全磁盘访问拦截时给用户的指引。
/// 2026-11-14：这是「Operation not permitted in subcomponent」的唯一真因 ——
/// TCC 是**进程级**校验，`do shell script ... with administrator privileges` 拿到 root
/// 也无法绕过，必须由邦信自己被授权。原来直接把 codesign stderr 抛给用户，看不懂。
fn app_mgmt_hint() -> String {
    "没有修改微信 App 的权限（被 macOS「App 管理」拦截）。\
     请到 系统设置 → 隐私与安全性 → App 管理，把「邦信」打开后重试。\
     勾选后必须重启邦信才生效 —— 直接点下面「已勾选，重启邦信继续」即可，\
     重启后回到这里再点一次重签。"
        .to_string()
}

/// 优雅退出微信（等同 ⌘Q）。重签前必须先退出：运行中的微信仍持有旧签名的内存映射，
/// 磁盘签名换了进程不换，用户会看到「重签成功但依然读不到消息」。
/// 顺序：AppleScript quit → 最多 3s 等自退 → TERM → 最多 2s → KILL。
fn quit_wechat() -> bool {
    if !wechat_running() {
        return true;
    }
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", "tell application \"WeChat\" to quit"])
        .output();
    for _ in 0..30 {
        if !wechat_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = Command::new("/usr/bin/pkill")
        .args(["-f", "WeChat.app/Contents/MacOS/WeChat"])
        .output();
    for _ in 0..20 {
        if !wechat_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = Command::new("/usr/bin/pkill")
        .args(["-9", "-f", "WeChat.app/Contents/MacOS/WeChat"])
        .output();
    for _ in 0..10 {
        if !wechat_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 重新打开微信（重签后自动拉起，用户不用自己去找 Dock 图标）。
fn open_wechat() {
    let _ = Command::new("/usr/bin/open").args(["-a", &wechat_app()]).output();
}

// ───────── 重签内核（2026-11-15 重做：先签副本，校验通过才换真包） ─────────
//
// 1.6.99 老实现的三个坑（线上实测）：
//  1) `codesign --force --deep --sign -` 只按主 bundle 的 entitlements 递归套下去，
//     嵌套 helper / XPC（WeChatAppEx 一类）被套上不属于自己的 entitlements →
//     微信主进程能跑、内部子进程起不来，表现就是「微信登录不了 / 起不来 / 读不到密钥」；
//  2) 签的是真包，签坏了没法退 —— 用户的微信就此不可用，只能重装；
//  3) 没备份，重装是唯一出路。
//
// 现在：ditto 到副本 → 副本上逐项签（每项保留自己的 entitlements，不用 --deep）
//      → codesign --verify --deep 校验 → 通过才原子换入真包；任何一步失败原包不动。
//      首次重签前把官方原版备份一份，随时可一键还原。

const BACKUP_NAME: &str = "WeChat-orig.app";
const STAGE_NAME: &str = "wx-resign-stage";

/// 邦信数据目录（备份/暂存放这，与 /Applications 同在数据卷，rename 瞬时）。
fn bx_data_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "无法定位当前用户家目录".to_string())?;
    let d = std::path::PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("com.x8fde.im");
    fs::create_dir_all(&d).map_err(|e| format!("创建数据目录失败：{e}"))?;
    Ok(d)
}

fn backup_path() -> Result<std::path::PathBuf, String> {
    Ok(bx_data_dir()?.join(BACKUP_NAME))
}

/// 是否有一份「官方原版」备份可直接还原（备份存在且本身未被重签）。
pub fn orig_backup_ok() -> bool {
    match backup_path() {
        Ok(p) => p.exists() && !wechat_resigned_at(&p.display().to_string()),
        Err(_) => false,
    }
}

/// 逐项签名脚本：不使用 --deep，每个嵌套 bundle / dylib 都抽自己的 entitlements 再签。
/// 由内向外（find -depth 后序），最后签主 bundle。
const SIGN_SH: &str = r#"/bin/sh
set -u
APP="$1"; MAINENT="$2"; W="$3"
: > "$W/sign.log"

sign_one() {
  _it="$1"
  _e="$W/item.ent"
  : > "$_e"
  /usr/bin/codesign -d --entitlements :- "$_it" >"$_e" 2>/dev/null
  if [ -s "$_e" ] && /usr/bin/grep -q "plist" "$_e" 2>/dev/null; then
    /usr/bin/codesign --force --sign - --timestamp=none --entitlements "$_e" "$_it" >>"$W/sign.log" 2>&1
  else
    /usr/bin/codesign --force --sign - --timestamp=none "$_it" >>"$W/sign.log" 2>&1
  fi
}

/usr/bin/find "$APP/Contents" -depth \
  \( -name '*.framework' -o -name '*.app' -o -name '*.xpc' -o -name '*.bundle' -o -name '*.appex' \) \
  > "$W/list_bundle" 2>/dev/null
while IFS= read -r it; do
  [ -n "$it" ] || continue
  sign_one "$it" || { echo "X8FAIL|bundle|$it"; exit 1; }
done < "$W/list_bundle"

/usr/bin/find "$APP/Contents" -depth \( -name '*.dylib' -o -name '*.so' \) > "$W/list_lib" 2>/dev/null
while IFS= read -r it; do
  [ -n "$it" ] || continue
  sign_one "$it" || { echo "X8FAIL|lib|$it"; exit 1; }
done < "$W/list_lib"

/usr/bin/find "$APP/Contents/MacOS" -depth -type f -perm -0100 > "$W/list_bin" 2>/dev/null
while IFS= read -r it; do
  [ -n "$it" ] || continue
  sign_one "$it" || { echo "X8FAIL|bin|$it"; exit 1; }
done < "$W/list_bin"

/usr/bin/codesign --force --sign - --timestamp=none --entitlements "$MAINENT" "$APP" >>"$W/sign.log" 2>&1 \
  || { echo "X8FAIL|main|$APP"; exit 1; }
exit 0
"#;

/// 换包脚本：原包先改名兜底，新包 ditto 进来后校验，失败自动回滚。原包永远留一条命。
const SWAP_SH: &str = r#"/bin/sh
set -u
STAGE="$1"; APP="$2"; OLD="/Applications/.WeChat.app.x8old"
if [ -e "$OLD" ]; then /bin/rm -rf "$OLD" || exit 11; fi
if ! /bin/mv "$APP" "$OLD" 2>/dev/null; then echo "X8FAIL|move"; exit 12; fi
if ! /usr/bin/ditto "$STAGE" "$APP" 2>/dev/null; then
  /bin/rm -rf "$APP"; /bin/mv "$OLD" "$APP"; echo "X8FAIL|ditto"; exit 13
fi
if ! /usr/bin/codesign --verify --deep "$APP" >/dev/null 2>&1; then
  /bin/rm -rf "$APP"; /bin/mv "$OLD" "$APP"; echo "X8FAIL|verify"; exit 14
fi
/bin/rm -rf "$OLD"
exit 0
"#;

/// 以 sh -c 跑一段脚本（用户态）。
fn run_sh(script: &str, a1: &str, a2: &str, a3: &str) -> Result<(), String> {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("x8sh")
        .arg(a1)
        .arg(a2)
        .arg(a3)
        .output()
        .map_err(|e| format!("执行脚本失败：{e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let o = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Err(if !o.is_empty() { o } else { e })
}

/// 提权跑同一段脚本（只在用户态被 TCC/权限拒绝时降级使用）。
/// 注意：AppleScript 的 `do shell script "..."` 里不能带换行，所以脚本先落临时文件，
/// 提权只执行 `/bin/sh <file>`。
fn run_sh_sudo(script: &str, a1: &str, a2: &str, a3: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join("x8_wx_sh");
    let _ = fs::create_dir_all(&dir);
    let f = dir.join(format!("swap_{}.sh", std::process::id()));
    fs::write(&f, script).map_err(|e| format!("写临时脚本失败：{e}"))?;
    let inner = format!(
        "/bin/sh {} {} {} {}",
        shell_quote(&f.display().to_string()),
        shell_quote(a1),
        shell_quote(a2),
        shell_quote(a3)
    );
    let osa = format!(
        "do shell script \"{}\" with administrator privileges",
        escape_for_osascript(&inner)
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &osa])
        .output()
        .map_err(|e| format!("提权执行失败：{e}"))?;
    let _ = fs::remove_file(&f);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.contains("User canceled") || err.contains("-128") {
            return Err("CANCELED".into());
        }
        return Err(err);
    }
    Ok(())
}

fn verify_app(app: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--verbose=2", app])
        .output()
        .map_err(|e| format!("校验调用失败：{e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let e = String::from_utf8_lossy(&out.stderr);
    Err(e.trim().lines().take(4).collect::<Vec<_>>().join("；"))
}

/// 重签微信。全流程：备份原版 → ditto 副本 → 副本签名 + 校验 → 退出微信 → 原子换入。
/// 任何一步失败，/Applications 上的微信保持原样。
pub fn resign_wechat() -> Result<String, String> {
    let app = wechat_app();
    if !Path::new(&app).exists() {
        return Err(format!(
            "没找到微信（{app}）。请先从 App Store 或官网安装微信，再回来重签。"
        ));
    }
    if !app_mgmt_granted() {
        return Err(format!("PERM|{}", app_mgmt_hint()));
    }

    // 1) 备份官方原版（只在「当前还没被重签」且「还没有备份」时做一次）。
    //    当前已经是重签状态就别备份了 —— 那是重签版，备份下来还原也没意义。
    let bak = backup_path()?;
    if !bak.exists() && !wechat_resigned() {
        let _ = Command::new("/usr/bin/ditto").arg(&app).arg(&bak).output();
    }

    // 2) 抽主 bundle 的 entitlements 并补 get-task-allow（缺它 lldb 挂不上）。
    let dump = Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-", &app])
        .output()
        .map_err(|e| format!("读取微信签名失败：{e}"))?;
    if !dump.status.success() {
        return Err(format!(
            "读取微信签名失败：{}",
            String::from_utf8_lossy(&dump.stderr)
        ));
    }
    let data = bx_data_dir()?;
    let ent = data.join("wx_main.entitlements");
    let work = data.join("wx-sign-work");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|e| format!("创建签名工作目录失败：{e}"))?;
    let ent_xml = ensure_get_task_allow(&String::from_utf8_lossy(&dump.stdout));
    fs::write(&ent, &ent_xml).map_err(|e| format!("写 entitlements 失败：{e}"))?;

    // 3) 复制到副本再签（真包全程不动）。
    let stage_root = data.join(STAGE_NAME);
    let _ = fs::remove_dir_all(&stage_root);
    fs::create_dir_all(&stage_root).map_err(|e| format!("创建暂存目录失败：{e}"))?;
    let stage_app = stage_root.join("WeChat.app");
    let cp = Command::new("/usr/bin/ditto")
        .arg(&app)
        .arg(&stage_app)
        .output()
        .map_err(|e| format!("复制微信到暂存区失败：{e}"))?;
    if !cp.status.success() {
        return Err("复制微信到暂存区失败（磁盘空间不足？微信约 1GB）".into());
    }

    // 4) 副本上逐项签名。
    let st = stage_app.display().to_string();
    if let Err(e) = run_sh(SIGN_SH, &st, &ent.display().to_string(), &work.display().to_string()) {
        let log = fs::read_to_string(work.join("sign.log")).unwrap_or_default();
        let tail: String = log.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
        let _ = fs::remove_dir_all(&stage_root);
        return Err(format!("在副本上签名失败（原微信未改动）：{e} {tail}"));
    }

    // 5) 校验副本：签名完整 + 无 Hardened Runtime + 带 get-task-allow。
    if let Err(e) = verify_app(&st) {
        let _ = fs::remove_dir_all(&stage_root);
        return Err(format!("复制出来的新签名校验不通过（原微信未改动）：{e}"));
    }
    if !wechat_resigned_at(&st) {
        let _ = fs::remove_dir_all(&stage_root);
        return Err("新签名没有达到可调试状态（要么还带 Hardened Runtime，要么 get-task-allow 没写进去）。原微信未改动。".into());
    }

    // 6) 退出微信后原子换入。换包失败会自动回滚成原包。
    if wechat_running() && !quit_wechat() {
        let _ = fs::remove_dir_all(&stage_root);
        return Err("微信正在运行且退不掉（可能弹着对话框）。请手动 ⌘Q 退出微信后重试。".into());
    }
    let swap = |sudo: bool| -> Result<(), String> {
        if sudo {
            run_sh_sudo(SWAP_SH, &st, &app, "")
        } else {
            run_sh(SWAP_SH, &st, &app, "")
        }
    };
    match swap(false) {
        Ok(()) => {}
        Err(e) if e.contains("User canceled") || e == "CANCELED" => {
            let _ = fs::remove_dir_all(&stage_root);
            return Err("已取消（未输入管理员密码），微信没有被改动。".into());
        }
        Err(_) => {
            // 用户态没权限动 /Applications → 提权再来一次（此时仍会自动回滚）
            if let Err(e2) = swap(true) {
                let _ = fs::remove_dir_all(&stage_root);
                if e2.contains("User canceled") || e2 == "CANCELED" {
                    return Err("已取消（未输入管理员密码），微信没有被改动。".into());
                }
                return Err(format!("换入新签名失败（已自动回滚为原微信）：{e2}"));
            }
        }
    }

    // 7) 清隔离属性（否则 Gatekeeper 下次启动会拦）+ 清暂存。
    let _ = Command::new("/usr/bin/xattr")
        .args(["-rd", "com.apple.quarantine", &app])
        .output();
    let _ = fs::remove_dir_all(&stage_root);

    Ok(done_msg())
}

/// 用备份还原官方微信（重签把它搞坏时的唯一退路）。
pub fn restore_wechat() -> Result<String, String> {
    let app = wechat_app();
    let bak = backup_path()?;
    if !bak.exists() {
        return Err("没有官方原版备份（这份备份只在首次重签前生成）。请到 weixin.qq.com/macwechat 重新下载安装，聊天记录不会丢。".into());
    }
    if app_mgmt_granted() && wechat_running() && !quit_wechat() {
        return Err("微信正在运行且退不掉，请手动 ⌘Q 退出微信后重试。".into());
    }
    let b = bak.display().to_string();
    let swap = |sudo: bool| -> Result<(), String> {
        if sudo {
            run_sh_sudo(SWAP_SH, &b, &app, "")
        } else {
            run_sh(SWAP_SH, &b, &app, "")
        }
    };
    match swap(false) {
        Ok(()) => {}
        Err(e) if e.contains("User canceled") || e == "CANCELED" => {
            return Err("已取消（未输入管理员密码），微信没有被改动。".into())
        }
        Err(_) => {
            if let Err(e2) = swap(true) {
                if e2.contains("User canceled") || e2 == "CANCELED" {
                    return Err("已取消（未输入管理员密码），微信没有被改动。".into());
                }
                return Err(format!("还原失败（已自动回滚）：{e2}"));
            }
        }
    }
    Ok("已还原为官方微信。要再读消息，重新点一次「一键重签」即可。".to_string())
}

/// 重签成功的统一文案（不再要求用户自己重启微信——由 wx_setup_run 自动拉起）。
fn done_msg() -> String {
    "微信已重签（副本签名 + 校验通过后换入；已移除 Hardened Runtime 并补上 get-task-allow）。".to_string()
}

/// 以当前用户经 LaunchAgent 拉起 wxgo（用户态，无需管理员权限）。
///
/// 为什么从 root LaunchDaemon 改回用户态 LaunchAgent：
/// 微信以当前登录用户运行；macOS 的 TCC/SIP 限制下，root 进程无法 lldb-attach 到
/// 用户态微信（密钥提取需在微信进程内断点捕获 passphrase）。同事在 mac 上用「用户态
/// 拉起 + lldb 挂用户微信」已验证成功，故本机也走用户态。LaunchAgent 在 gui/$UID 域，
/// 普通用户即可 load，令牌仍只经 0600 临时文件注入。
///
/// 升级兼容：从 1.6.83（root LaunchDaemon）升级时，旧 root 守护仍占着端口、会挡新实例
/// 绑定。故先尝试用管理员密码 bootout 旧 system 域守护（仅当 /Library/LaunchDaemons 下
/// plist 仍存在才弹窗，弹一次后 plist 即删，之后不再弹）；再 bootout 同名 user agent，
/// 保证单实例 + 新令牌生效。令牌只经 0600 临时文件（X8TOKEN_FILE）注入，绝不进 argv /
/// plist 明文 / 环境变量值；wxgo 读后即删，落盘窗口极短。本机不持子进程句柄，靠端口探活。
pub fn launch_user(
    bin: &Path,
    args: &[String],
    token: &str,
    port: u16,
    ready: &Path,
) -> Result<(), String> {
    // 0) 升级兼容：若存在旧版 root LaunchDaemon，需管理员一次性清理（仅首次弹一次密码）。
    let old_daemon = "/Library/LaunchDaemons/com.x8fde.im.wxgo.plist";
    if Path::new(old_daemon).exists() {
        let inner = format!(
            "launchctl bootout system/com.x8fde.im.wxgo 2>/dev/null; rm -f {daemon}; true",
            daemon = shell_quote(old_daemon)
        );
        let osa = format!(
            "do shell script \"{}\" with administrator privileges",
            escape_for_osascript(&inner)
        );
        let out = Command::new("/usr/bin/osascript")
            .args(["-e", &osa])
            .output()
            .map_err(|e| format!("清理旧版后台微信插件失败：{e}"))?;
        if !out.status.success() {
            return Err(
                "清理旧版（root）后台微信插件被拒。请允许管理员密码，或手动执行：\
                 sudo launchctl bootout system/com.x8fde.im.wxgo 并删除 \
                 /Library/LaunchDaemons/com.x8fde.im.wxgo.plist 后重试。"
                    .into(),
            );
        }
    }
    // 同名 user agent 也先清掉（无需管理员），保证单实例 + 新令牌生效。
    let uid = current_uid();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}/com.x8fde.im.wxgo")])
        .output();
    // 🔴 2026-11-12：bootout 是异步的 —— 带 KeepAlive 的 job 需要先等它自己退出。
    // 不等就 bootstrap 会拿到「5: Input/output error」，且日志文件都不建
    // （实测 2026-11-12：W消息 报「新版组件启动失败（启动超时）」，日志为空）。
    // 这里轮询到 job 真正从 gui 域消失，最多 3s。
    for _ in 0..30 {
        if !agent_loaded(uid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // 1) 令牌写入 0600 临时文件（当前用户可读），经 X8TOKEN_FILE 注入。
    let tok_path = std::env::temp_dir().join(format!("x8_wx_token_{port}.txt"));
    fs::write(&tok_path, format!("{token}\n"))
        .map_err(|e| format!("写令牌文件失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tok_path, fs::Permissions::from_mode(0o600));
    }

    // 2) 生成 LaunchAgent plist，装入 ~/Library/LaunchAgents（用户态，免管理员）。
    let home = home_dir().ok_or_else(|| "无法定位当前用户家目录（HOME 为空）".to_string())?;
    let agent_dir = Path::new(&home).join("Library").join("LaunchAgents");
    fs::create_dir_all(&agent_dir).map_err(|e| format!("建 LaunchAgents 目录失败：{e}"))?;
    let plist_path = agent_dir.join("com.x8fde.im.wxgo.plist");
    let work_dir = bin
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    let log_path = format!("/tmp/x8_wx_{port}.log");
    let plist = build_wxgo_plist(bin, args, &tok_path.display().to_string(), &work_dir, &log_path);
    fs::write(&plist_path, plist).map_err(|e| format!("写 LaunchAgent plist 失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&plist_path, fs::Permissions::from_mode(0o644));
    }

    // 3) 以当前用户 load（无需管理员）。优先用现代 bootstrap gui 域；失败重试一次，
    //    再失败退用旧式 load。2026-11-12：单次 bootstrap 撞上 launchd 的异步收尾
    //    就会报「5: Input/output error」，重试是必要且安全的（job 已 bootout 干净）。
    let label = format!("gui/{uid}/com.x8fde.im.wxgo");
    let mut diag = String::new();
    let mut bootstrap_ok = false;
    for attempt in 0..2 {
        let out = Command::new("/bin/launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                &plist_path.display().to_string(),
            ])
            .output();
        if matches!(out, Ok(ref o) if o.status.success()) {
            bootstrap_ok = true;
            break;
        }
        if let Ok(o) = &out {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !msg.is_empty() {
                diag.push_str(&format!("bootstrap 失败（第 {} 次）：{msg}\n", attempt + 1));
            }
        }
        if attempt == 0 {
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
    }
    if !bootstrap_ok {
        let _ = Command::new("/bin/launchctl")
            .args(["load", "-w", &plist_path.display().to_string()])
            .output();
    }

    // 3.5) 真正拉起：plist RunAtLoad=false，必须显式 kickstart，否则只注册不运行
    //      （表现为「无日志 + 端口超时」——这是 1.6.84 失败的根因）。
    let ks = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &label])
        .output();
    if let Ok(o) = &ks {
        if !o.status.success() {
            diag.push_str(&format!(
                "kickstart 失败：{}\n",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
    }

    // 4) 等插件就绪（探测前先 settle 500ms，让 launchd 真正拉起进程；总上限 30s）。
    //    2026-11-12：判据从「端口能连上」换成「插件写了 ready 标记」为主，
    //    端口探测降级为兜底（旧版插件不写标记）。原实现 12s 端口轮询会把
    //    「抢单实例锁 / 争端口 / 大库初始化」的慢启动误判成启动失败 → 回退旧版。
    std::thread::sleep(Duration::from_millis(500));
    let addr = format!("127.0.0.1:{port}")
        .parse::<SocketAddr>()
        .map_err(|e| format!("端口解析失败：{e}"))?;
    let tries = BOOT_TIMEOUT_MS / PROBE_INTERVAL_MS;
    let mut spawned_fallback = false;
    for i in 0..tries {
        std::thread::sleep(Duration::from_millis(PROBE_INTERVAL_MS));
        if ready.is_file() {
            let _ = fs::remove_file(&tok_path);
            return Ok(());
        }
        // 端口探测每秒一次即可，别每 100ms 连一次浪费 fd。
        if i % 10 == 9 && TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            // 端口已起：wxgo 已读过令牌文件，本机删掉它缩短落盘窗口。
            let _ = fs::remove_file(&tok_path);
            return Ok(());
        }
        // 🔴 最后一招（2026-11-12）：launchd 域彻底坏掉时（bootstrap/kickstart 全失败），
        // 5s 后还在原地就绕过 launchd，直接以当前用户 spawn —— 宿主本来就是登录用户，
        // 与微信同用户，lldb 照样能 attach。宁可少一层 KeepAlive 自愈（由宿主看门狗
        // 30s 兜），也不能让用户卡在「组件起不来」上。
        if i == 50 && !spawned_fallback {
            spawned_fallback = true;
            match Command::new(bin)
                .args(args)
                .env("X8TOKEN_FILE", &tok_path)
                .current_dir(bin.parent().unwrap_or(Path::new("/tmp")))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => diag.push_str(&format!("launchd 未拉起，已改为直接启动 pid={}\n", c.id())),
                Err(e) => diag.push_str(&format!("直接启动也失败：{e}\n")),
            }
        }
    }
    // 超时：把真实死因（bootstrap/kickstart 报错 / 签名拦截 / panic）从日志 + 诊断拼进错误。
    let tail = read_log_tail(&log_path, 40);
    let _ = fs::remove_file(&tok_path);
    let mut reason = format!(
        "微信插件启动超时（{}s 内未就绪，端口 {port} 未监听）。{tail}",
        BOOT_TIMEOUT_MS / 1000
    );
    if !diag.trim().is_empty() {
        reason.push_str(&format!("\nlaunchctl 诊断：\n{diag}"));
    }
    if tail.contains("无日志") {
        reason.push_str(
            "\n若反复出现：可能是 1.6.83 的旧 root 守护没清掉，终端执行 \
             sudo launchctl bootout system/com.x8fde.im.wxgo 并删除 \
             /Library/LaunchDaemons/com.x8fde.im.wxgo.plist 后重开邦信。",
        );
    }
    Err(reason)
}

/// 当前用户家目录（HOME）。
fn home_dir() -> Option<String> {
    let h = std::env::var("HOME").ok()?;
    if h.trim().is_empty() {
        None
    } else {
        Some(h)
    }
}

/// 该 LaunchAgent 是否还留在当前用户的 gui 域里。
/// `launchctl print` 成功 = job 还在（哪怕进程已死，KeepAlive 也会重拉）。
fn agent_loaded(uid: u32) -> bool {
    Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{uid}/com.x8fde.im.wxgo")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 当前用户 uid（用于 gui/<uid> LaunchAgent 域）。
pub fn current_uid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// 构造 wxgo 的 LaunchAgent plist（XML）。令牌只以文件路径（X8TOKEN_FILE）传递，
/// 绝不把明文写进 plist；--parent-watch 被刻意剔除（mac 无父管道，靠 HTTP 优雅退出回收）。
fn build_wxgo_plist(
    bin: &Path,
    args: &[String],
    token_file: &str,
    work_dir: &str,
    log: &str,
) -> String {
    let mut prog = String::new();
    prog.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&bin.display().to_string())
    ));
    for a in args {
        if a == "--parent-watch" {
            continue; // mac 无父管道，靠 HTTP 优雅退出 + kill_by_port 回收
        }
        prog.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.x8fde.im.wxgo</string>
    <key>ProgramArguments</key>
    <array>
{prog}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>X8TOKEN_FILE</key>
        <string>{token_file}</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>{work_dir}</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>RunAtLoad</key>
    <false/>
    <!-- 只在「异常退出」（panic / 崩溃 / 被杀）时由 launchd 重新拉起；
         正常退出（/api/app/shutdown → exit 0）不复活，避免与 stop_with 打架。
         ThrottleInterval 防崩溃循环狂刷。2026-11-11：原先 KeepAlive=false，
         插件一崩就永久死掉，界面只剩「状态未知 / 插件无响应」。 -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
</dict>
</plist>
"#,
        prog = prog,
        token_file = xml_escape(token_file),
        work_dir = xml_escape(work_dir),
        log = xml_escape(log),
    )
}

/// 读日志末尾 N 行（超时兜底诊断用）。
fn read_log_tail(path: &str, n: usize) -> String {
    let Ok(s) = fs::read_to_string(path) else {
        // 文件不存在 = 进程压根没被 spawn（launchd 连重定向都没建）。
        // 2026-11-12：原文案误导向 Gatekeeper；真正常见原因是 launchd 域的
        // bootstrap 失败（Input/output error）或 plist 指向的可执行文件不可用。
        return "(无日志文件，说明插件进程没被真正拉起)".to_string();
    };
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        "(日志为空，插件刚启动就退出了，多半是抢单实例锁失败或参数不认)".to_string()
    } else {
        format!("日志末尾：\n{tail}")
    }
}

/// XML 文本转义（plist 里路径可能含 & < > "）。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 按端口反查并杀掉用户态 wxgo（优雅退出由调用方先发 /api/app/shutdown）。
pub fn kill_by_port(port: u16) {
    // 优先用 launchctl 按 label 停掉 user agent（优雅，不靠猜 PID）。
    let uid = current_uid();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}/com.x8fde.im.wxgo")])
        .output();
    // 反查监听该端口的 PID，逐个 TERM（含残留 / 同名其他实例）。
    let out = Command::new("/usr/sbin/lsof")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output();
    if let Ok(o) = out {
        for pid in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            let _ = Command::new("/bin/kill").args(["-TERM", pid]).output();
        }
    }
    // 兜底：按进程名收（lsof 看不到时）
    let _ = Command::new("/usr/bin/pkill")
        .args(["-f", "wxadapter"])
        .output();
    let _ = Command::new("/usr/bin/pkill")
        .args(["-f", "wxgo"])
        .output();
}

/// 单引号包裹（POSIX sh 标准转义，内部单引号用 '\'' 处理）；路径含空格也安全。
fn shell_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// 嵌入 `do shell script "..."` 的双引号字符串时，转义反斜杠与双引号。
fn escape_for_osascript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ─────────────────────────── Tauri 命令 ───────────────────────────

/// 前端向导轮询：当前 mac 读微信的三项前置条件 + 是否可一键还原。
#[tauri::command]
pub fn wx_setup_state() -> Value {
    let resigned = wechat_resigned();
    let running = wechat_running();
    json!({
        "supported": true,
        "fda": fda_granted(),
        "app_mgmt": app_mgmt_granted(),
        "resigned": resigned,
        "wechat_running": running,
        "ready": resigned && running,
        "has_backup": orig_backup_ok(),
    })
}

/// 捕获密钥前「自动关微信 → 等一会 → 再打开」。
///
/// 为什么需要：密钥是微信**登录那一刻**由 PBKDF2 派生的，keyext 只能在登录流程里
/// 断点捕获。让用户自己去「设置 → 退出登录 → 重新登录」对小白太重（还经常点错）。
/// 这里代劳：优雅退出微信、等 1.2 秒、再拉起 —— 微信重开必停在扫码登录界面，
/// 扫一下就产生密钥派生，捕获即可命中。
#[tauri::command]
pub fn wx_wechat_relogin() -> Value {
    if wechat_running() && !quit_wechat() {
        return json!({ "ok": false, "err": "微信退不掉（可能弹着对话框），请手动 ⌘Q 关掉微信后再点一次" });
    }
    std::thread::sleep(Duration::from_millis(1200));
    open_wechat();
    for _ in 0..80 {
        if wechat_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    json!({ "ok": true, "running": wechat_running() })
}

/// 一键还原官方微信（重签把它搞坏时的退路，用首次重签前备份的原版覆盖回去）。
#[tauri::command]
pub fn wx_restore_wechat() -> Value {
    match restore_wechat() {
        Ok(msg) => {
            if !wechat_running() {
                open_wechat();
            }
            json!({ "ok": true, "msg": msg })
        }
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/// 前端点「一键重签」：以管理员密码提权重签微信。重签后若微信仍在跑，提示重启。
#[tauri::command]
pub fn wx_setup_run() -> Value {
    match resign_wechat() {
        Ok(msg) => {
            // 重签前由 resign_wechat 主动退出了微信，这里自动拉起，用户不必自己找图标。
            if !wechat_running() {
                open_wechat();
                for _ in 0..50 {
                    if wechat_running() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let running = wechat_running();
            json!({
                "ok": true,
                "resigned": true,
                "wechat_running": running,
                "msg": if running {
                    format!("{msg}微信已自动重新打开，登录后点「开始读取」即可。")
                } else {
                    format!("{msg}微信没能自动打开，请手动打开并登录微信。")
                }
            })
        }
        Err(e) => {
            let perm = e.starts_with("PERM|");
            json!({
                "ok": false,
                "perm": if perm { "app_mgmt" } else { "" },
                "err": e.trim_start_matches("PERM|"),
            })
        }
    }
}
