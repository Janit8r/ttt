# IPTV Proxy 项目源代码功能总结报告

> 分析对象：`ttt-main`（Cargo 包名 `iptv-proxy` v1.0.0）
> 分析范围：`src/main.rs`（413 行，全部业务代码）、`Cargo.toml`、3 个 Shell 脚本、1 个 GitHub Actions 工作流、`README.md`
> 分析视角：CodeReviewExpert（架构梳理 + 代码审查）
> 报告日期：2026-09-24

---

## 一、项目概览

### 1.1 一句话定位

这是一个**基于 Cloudflare Pingora 框架的单文件 HTTP 反向代理**，部署在家庭/企业内网的 OpenWrt 路由器上，专门用于**把外部 IPTV/HLS 直播源改写成内网可播放的地址**，从而绕过源站的 Referer 校验、IP 地域限制，并让内网播放器获得统一的访问入口。

### 1.2 工作模型

```
播放器(TV/盒子)  ──►  路由器上的 iptv-proxy:8080  ──►  外网 IPTV 源站
                         ↑ 改写 m3u8 / 注入 Referer
```

核心机制是 **URL 参数化代理**：所有目标地址都通过 `?url=<URL编码后的目标地址>` 传入，代理解析后向源站发起请求，并在返回 m3u8 播放列表时，**把列表里的每一条分片 URL 再改写回指向代理自身**，形成递归代理链。

### 1.3 文件清单与规模

| 文件 | 行数 | 类型 | 作用 |
|------|------|------|------|
| `src/main.rs` | 413 | Rust | **全部业务代码**（唯一的源文件） |
| `Cargo.toml` | 34 | 配置 | 依赖声明 + Release 编译优化配置 |
| `README.md` | 302 | 文档 | 使用说明、部署、调优（**部分内容已过期**） |
| `deploy-openwrt.sh` | 161 | Shell | 交叉编译 + scp + 生成 procd init 脚本 + 内核参数调优 |
| `deploy.sh` | 53 | Shell | 早期 Linux/systemd 部署脚本（含明文密码） |
| `test-performance.sh` | 104 | Shell | wrk 压力测试 / 内存与连接数采集 |
| `.github/workflows/build-openwrt-ipq60xx.yml` | 118 | CI | musl 静态交叉编译 + 打包 OpenWrt 安装包 + 发布 Release |
| `Cargo.lock` | — | — | **缺失**（未提交，构建不可复现） |

---

## 二、整体架构

### 2.1 分层架构

```
┌─────────────────────────────────────────────────────────────┐
│  接入层 (Downstream)                                         │
│  Pingora Server 监听 0.0.0.0:8080，HTTP/1.1                  │
│  地址形态: /?url=<encoded> | /health | /favicon.ico          │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  控制层 (ProxyHttp trait 实现 —— IptvProxy)                  │
│  ┌────────────┬──────────────┬──────────────┬─────────────┐ │
│  │request_    │upstream_peer │upstream_     │response_    │ │
│  │filter      │              │request_filter│filter       │ │
│  │ 路由/鉴权  │ 选上游Peer   │ 伪造请求头   │ 改响应头    │ │
│  └────────────┴──────────────┴──────────────┴─────────────┘ │
│  ┌────────────────────────┐  ┌──────────────────────────┐   │
│  │response_body_filter    │  │logging                   │   │
│  │ m3u8 逐行改写(核心)    │  │ 访问/错误日志            │   │
│  └────────────────────────┘  └──────────────────────────┘   │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  上游层 (Upstream)                                           │
│  HttpPeer { host, port, TLS, SNI }，HTTP/HTTPS 自动识别      │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 请求生命周期（Pingora ProxyHttp 六阶段）

Pingora 的 `ProxyHttp` trait 定义了代理的完整生命周期，本项目实现了其中 6 个钩子：

| # | 钩子 | 本项目的职责 | 返回 `Ok(true)` 含义 |
|---|------|-------------|---------------------|
| 1 | `new_ctx` | 为每请求创建 `ProxyContext` 空上下文 | — |
| 2 | `request_filter` | 路由分发：健康检查 / favicon / 解析 `url=` 参数 | 已直接响应客户端，不再走上游 |
| 3 | `upstream_peer` | 根据 `ctx.target_url` 计算上游 Host:Port 与是否 TLS | — |
| 4 | `upstream_request_filter` | 重写 URI、**注入 UA/Referer**、转发鉴权头 | — |
| 5 | `response_filter` | 改写 3xx `Location`、修正 `Content-Type`、删 `Content-Length` | — |
| 6 | `response_body_filter` | **m3u8 播放列表逐行改写为代理地址** | — |
| 7 | `logging` | 记录 `客户端IP 方法 路径 状态码` 与错误 | — |

---

## 三、核心类与数据结构

### 3.1 `ProxyConfig`（配置载体，`Clone`）

```rust
pub struct ProxyConfig {
    pub local_ip: String,   // 本机内网 IP，用于拼接改写后的代理地址
    pub bind_port: u16,     // 监听端口，同上
}
```

- **作用**：保存"我是谁"的信息。m3u8 改写时必须知道代理自身的可达地址，否则播放器拿到的改写 URL 无法回访。
- **来源**：`main()` 从 `-Li <ip>` 命令行参数 → `LOCAL_IP` 环境变量 → 默认值 `192.168.1.1` 三级降级；端口由 `BIND_ADDR`（默认 `0.0.0.0:8080`）解析。
- 实现了 `Clone`，因为 `IptvProxy` 被 Pingora 多线程共享，配置需复制进每个 worker。

### 3.2 `ProxyContext`（每请求上下文，状态机核心）

```rust
pub struct ProxyContext {
    target_url: Option<url::Url>,  // 解析后的完整目标 URL
    is_m3u8: bool,                 // 是否为播放列表（决定 body 是否改写）
    base_url: Option<String>,      // scheme://authority/目录/      → 拼相对路径
    origin_base: Option<String>,   // scheme://authority            → 拼根绝对路径
    needs_jpeg_fix: bool,          // 是否启用 ".jpeg 伪装成 TS" 兼容模式
}
```

- **作用**：在 6 个钩子之间传递请求级状态。这是典型的"上下文对象"模式 —— 因为 Pingora 的钩子签名是分离的，无法用局部变量共享数据。
- **生命周期**：`new_ctx()` 创建 → `request_filter` 填充 → 后续钩子消费 → 请求结束丢弃。

### 3.3 `IptvProxy`（核心代理逻辑，`ProxyHttp` 实现者）

持有 `config: ProxyConfig`，实现 `ProxyHttp<CTX = ProxyContext>`。所有业务规则都写在它的钩子和两个辅助函数里。

### 3.4 关键常量

| 常量 | 值 | 用途 |
|------|-----|------|
| `DEFAULT_REFERER` | `https://missav.ws/dm242/cn` | 客户端未带 Referer 时的兜底值（绕过源站防盗链） |
| `DEFAULT_USER_AGENT` | Chrome 120 / Windows 10 | 客户端未带 UA 时的兜底值（伪装浏览器） |
| `MEDIA_EXTS` | `.ts .m3u8 .m3u .mp4 .m4s .m4a .aac .mp3 .ogg .opus .vtt .srt .jpeg .jpg .png .key` | 判断一行文本是否为媒体资源 URL |

---

## 四、核心函数逐项解析

### 4.1 `main()` —— 启动入口

```
env_logger 初始化（默认 info 级，毫秒时间戳）
  ↓
解析参数: -Li <ip>  →  LOCAL_IP  →  "192.168.1.1"
解析环境变量: BIND_ADDR → "0.0.0.0:8080"，从中切出 bind_port
  ↓
Server::new(Some(Opt { upgrade:false, daemon:false, ... }))
  ↓
http_proxy_service(&server.configuration, IptvProxy::new(config))
  .add_tcp(&bind_addr)
  ↓
server.run_forever()   // 阻塞
```

**要点**：`daemon: false` 表示进程前台运行，守护由外部（procd/systemd）负责；`upgrade: false` 关闭 Pingora 的热升级能力。

### 4.2 `request_filter()` —— 路由与参数解析（第一道关卡）

三条分支：

1. **`GET /health` 或 `GET /`（无 query）** → 直接返回 `200 OK` + 正文 `"OK"`，用于健康探测。
2. **`GET /favicon.ico`** → 直接返回 `404`，避免播放器图标请求打到上游。
3. **含 `url=<encoded>` 参数** → 进入代理主流程：
   - `urlencoding::decode` 解码（失败 → 400）
   - `url::Url::parse` 解析（失败 → 400）
   - 用 `url.path()` 判定 `.m3u8`/`.m3u` → 设置 `ctx.is_m3u8`
   - 检测 `real_ext=jpeg` 参数 → 置 `ctx.needs_jpeg_fix` 并**从 URL 中剔除该参数**（避免污染上游请求）
   - 计算 `origin_base` 与 `base_url`（后者取路径最后一个 `/` 之前的目录）
   - 改写下游请求的 raw_path 与 `Host` 头
   - 返回 `Ok(false)` → **继续走上游**
4. **其他** → `Err(HTTPStatus(400))`，提示 `Use /?url=<encoded_target>`

### 4.3 `upstream_peer()` —— 上游节点选择

```
host ← target.host_str()（缺省 "localhost"）
port ← target.port() 或按 scheme 推断（https→443 / http→80）
tls  ← scheme == "https"
返回 HttpPeer::new((host, port), tls, host)   // 第三参数为 SNI
```
**要点**：SNI 与 Host 一致，避免 HTTPS 握手失败；**未设置连接/读超时**，依赖 Pingora 默认值。

### 4.4 `upstream_request_filter()` —— 上游请求头伪造（绕过防盗链的关键）

按优先级构造发往源站的请求：

| 头部 | 策略 |
|------|------|
| **URI** | 一律用 `ctx.target_url` 的 `path + query`；若 `needs_jpeg_fix` 则把路径中的 `.ts` 替换为 `.jpeg` |
| **Host** | 目标 URL 的 authority |
| **User-Agent** | **客户端原值优先**，缺失时用 `DEFAULT_USER_AGENT` |
| **Referer** | **客户端原值优先**，缺失时用 `DEFAULT_REFERER` |
| `origin` / `cookie` / `authorization` / `x-forwarded-for` | 原样透传（有则传） |
| `Accept` | 强制 `*/*` |
| `Accept-Encoding` | **m3u8 时移除**（必须拿明文才能改写 body） |

并打印 `Upstream request headers -> UA: ..., Referer: ...` 调试日志。

### 4.5 `response_filter()` —— 响应头修正

1. **重定向改写**：对 `301/302/307/308`，把 `Location` 解析为绝对 URL（绝对直接用；相对用 `ctx.target_url.join()` 解析），再包成 `/?url=<encoded>` 重新指回代理，并记录 `Rewritten redirect: A -> B`。
2. **`needs_jpeg_fix` 修正**：删除原 `Content-Type`，强制设为 `video/mp2t`，并移除 `Content-Disposition`（防止播放器下载而非播放）。
3. **m3u8 移除 `Content-Length`**：因为后续 body 长度会变，避免长度不匹配导致截断。

### 4.6 `response_body_filter()` —— m3u8 改写（**最核心的算法**）

仅当 `ctx.is_m3u8` 时执行，逐行扫描播放列表文本：

```
for line in content.lines():
    ├─ 以 '#' 开头（标签行）
    │    ├─ 是 #EXTINF / #EXT-X-STREAM-INF / #EXT-X-I-FRAME-STREAM-INF
    │    │     → 暂存为 pending_tag，等下一行 URI 出现时再一起输出
    │    └─ 其他标签 → 原样输出
    ├─ 空行 → 先冲刷 pending_tag，再输出空行
    ├─ http:// 或 https:// 开头（绝对 URL）
    │    → 冲刷 tag，编码后改写为 http://{local_ip}:{port}/?url={encoded}
    ├─ is_likely_media_resource(line)（相对路径且像媒体资源）
    │    → 冲刷 tag，用 base_url / origin_base 补全为绝对 URL
    │      ├─ 若以 .jpeg 结尾 → 反转为 .ts 并追加 real_ext=jpeg
    │      └─ 否则直接编码
    │    → 改写为 http://{local_ip}:{port}/?url={encoded}
    └─ 其他（无法识别的行）→ 丢弃 pending_tag
```

**`pending_tag` 机制的设计意图**：保证 `#EXTINF` 与其后的分片 URL 在输出中保持相邻配对；如果下一行不是可识别的 URI，则该标签被丢弃，避免产生"有标签无地址"的畸形播放列表。

### 4.7 两个辅助（判别）函数

**`is_likely_media_resource(line) -> bool`**
1. 以 `/` 开头（根相对路径）→ 直接判定为资源。
2. 否则去掉 `?` 之后的查询串，逐个比对 `MEDIA_EXTS` 后缀。
3. 命中后缀后，取文件名部分：**若文件名为空或全为数字 → 判定 false（不改写）**。

> ⚠️ 第 3 条是本函数最大的争议点，详见「问题清单 P1-3」。

**`tag_requires_uri(line) -> bool`**
判定该标签后面必须跟随一个 URI 行，即只对 `#EXTINF:`、`#EXT-X-STREAM-INF:`、`#EXT-X-I-FRAME-STREAM-INF:` 三种启用 `pending_tag` 机制。

### 4.8 `logging()` —— 访问日志

输出 `客户端IP 方法 路径 - Status:状态码`，出错时追加 `Error:{:?}` 到 error 级别。

---

## 五、业务流程详解

### 5.1 流程 A：健康探测（最简路径）

```
播放器 ──GET /health──► 代理
代理   ──200 "OK"────► 播放器      （完全不触达上游）
```

### 5.2 流程 B：m3u8 播放列表获取与改写（主线业务）

```
① 播放器请求 http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2Fxxx%2Findex.m3u8
② request_filter: 解码 → 解析 → is_m3u8=true → 算出 base_url/origin_base
③ upstream_peer:  Peer = (116.199.7.27:8006, TLS=false)
④ upstream_request_filter: 移除 Accept-Encoding，注入 UA/Referer
⑤ 源站返回 m3u8 明文
⑥ response_filter: 删 Content-Length
⑦ response_body_filter: 每一行分片 URL → http://192.168.1.3:8080/?url=<编码后的分片地址>
⑧ 播放器收到的列表里，所有分片都指向代理自身
⑨ 播放器请求第 2 步生成的分片地址 → 回到本流程的「非 m3u8 分支」（见流程 C）
```

### 5.3 流程 C：TS / 媒体分片透明转发

```
播放器 ──GET /?url=<...segment.ts>──► 代理
                                       │ is_m3u8=false
                                       │ body 不改写，字节流零拷贝透传
                                       ▼
                                    源站 → 代理 → 播放器
```
分片数据不经过任何解析，是性能最优路径（也是 README 所说"零拷贝"的实际体现：`Bytes` 直接透传）。

### 5.4 流程 D：`real_ext=jpeg` 兼容模式（应对伪装后缀的源站）

某些源站把真实的 MPEG-TS 分片命名为 `.jpeg`（用于绕过审查或 CDN 规则），播放器却按 `.ts` 语义解析。代理的做法是**双向翻译**：

```
入站：?url=...segment.ts?real_ext=jpeg
      → 剔除 real_ext 参数，把路径 .ts 替换为 .jpeg  后发给源站
出站：Content-Type 强制为 video/mp2t，删除 Content-Disposition
m3u8 改写：列表里的 .jpeg 反转为 .ts 并补回 real_ext=jpeg，
           使播放器与代理之间始终用 ".ts" 语义通信
```

### 5.5 流程 E：上游重定向跟随

```
源站 302 → Location: /new/path/list.m3u8
代理      → 解析为绝对 URL
          → Location: /?url=http%3A%2F%2F...%2Fnew%2Fpath%2Flist.m3u8
播放器    → 自动回落到代理，继续被代理
```
这保证了**重定向不会"逃逸"出代理隧道**，是本设计里比较周到的一处。

---

## 六、模块间依赖关系

### 6.1 内部依赖（本项目只有一个模块）

```
main()
  └─► ProxyConfig::new()  ──►  IptvProxy::new(config)
                                    └─► ProxyHttp 实现
                                          ├─ new_ctx() ──► ProxyContext::new()
                                          ├─ request_filter()      ──► 写 ctx
                                          ├─ upstream_peer()       ──► 读 ctx.target_url
                                          ├─ upstream_request_filter() ──► 读 ctx
                                          ├─ response_filter()     ──► 读 ctx
                                          ├─ response_body_filter()──► 读 ctx + config
                                          │       └─► is_likely_media_resource()
                                          │       └─► tag_requires_uri()
                                          └─ logging()
```

**依赖特点**：高度内聚、无循环依赖。所有模块通过 `ProxyContext` 单向传递状态，`config` 只读共享。

### 6.2 外部依赖（实际使用 vs 声明）

| 依赖 | 声明 | 实际使用 | 用途 |
|------|------|---------|------|
| `pingora` / `pingora-core` | ✅ | ✅ 6 处 | 代理框架本体（Git rev `d9e6d7a`，features: `lb`, `rustls`） |
| `async-trait` | ✅ | ✅ | `ProxyHttp` 的 async 方法需要 |
| `url` | ✅ | ✅ 27 处 | URL 解析、`join()` 解析相对重定向 |
| `urlencoding` | ✅ | ✅ 5 处 | `url=` 参数的编解码 |
| `http` | ✅ | ✅ 7 处 | `Uri` 类型，用于 `set_uri` |
| `bytes` | ✅ | ✅ 4 处 | `Bytes` 零拷贝缓冲区 |
| `log` / `env_logger` | ✅ | ✅ | 日志 |
| `regex` | ✅ | ❌ **未使用** | — |
| `once_cell` | ✅ | ❌ **未使用** | — |
| `ahash` | ✅ | ❌ **未使用** | — |
| `memchr` | ✅ | ❌ **未使用** | README 宣称的"SIMD 加速"实际不存在 |
| `num_cpus` | ✅ | ❌ **未使用** | — |
| `reqwest` | ✅ | ❌ **未使用** | — |
| `tokio` | ✅ | ❌ **未使用** | Pingora 已自带运行时 |

**结论：14 个声明依赖中 7 个是死依赖**，直接拖长编译时间、增大二进制体积。

### 6.3 构建与部署依赖链

```
src/main.rs
   │  cargo build --release --target aarch64-unknown-linux-musl
   ▼
静态二进制（lto=fat, opt-level=3, strip=true, panic=abort）
   ├──────────────► deploy-openwrt.sh  ──► procd init 脚本 + sysctl 调优 ──► OpenWrt 路由器
   ├──────────────► .github/workflows  ──► tar.gz 安装包 + GitHub Release
   └──────────────► deploy.sh          ──► systemd 服务 ──► 通用 Linux 主机
```

**Release 编译配置**（`Cargo.toml`）：`lto="fat"` + `codegen-units=1` + `opt-level=3` + `strip=true` + `panic="abort"` —— 极致体积与性能优化，但 `panic=abort` 与代码中的 `expect()` 组合会放大风险（见 P0-2）。

---

## 七、代码质量评估

### 7.1 优点

1. **架构清晰**：严格遵循 Pingora 的生命周期钩子划分，职责单一，每个钩子只做一件事。
2. **上下文设计合理**：`ProxyContext` 预计算 `base_url` / `origin_base`，避免在热路径重复解析 URL。
3. **重定向处理周到**：`Location` 回包成代理地址，防止流量逃逸。
4. **请求头策略务实**：客户端原值优先、兜底值补位，既尊重播放器又保证兼容性。
5. **`real_ext=jpeg` 双向翻译**：针对真实源站怪癖的实用工程解法。
6. **日志可观测性较好**：关键节点（解码 URL、上游信息、实际发出的 UA/Referer、改写行数、重定向映射）都有 info 级日志，便于排障。

### 7.2 问题清单（按严重度）

#### 🔴 P0 —— 阻断性 / 高危

| ID | 问题 | 位置 | 说明 |
|----|------|------|------|
| **P0-1** | **开放的 SSRF / 任意代理（无白名单）** | `request_filter` | 任何能访问 8080 的人都可让代理请求任意 URL，包括 `http://127.0.0.1:80`、`http://192.168.1.1/cgi-bin/...`（路由器管理后台）。部署在内网时等同于**内网横向穿越入口**。建议：接入域名白名单 / 目标端口黑名单 / 拒绝 loopback 与内网段，或至少增加简单 token 鉴权。 |
| **P0-2** | **`panic="abort"` + `expect()` = 进程级 DoS** | `response_body_filter` L280-281 | `ctx.base_url.as_ref().expect("base_url missing")` 与 `origin_base.expect(...)` 一旦为 `None`，release 构建下**整个进程直接 abort**，所有在途连接瞬间断开。应改为 `if let (Some(base), Some(origin)) = ... else { return Ok(None) }` 的优雅降级。 |
| **P0-3** | **`README.md` 与代码严重脱节** | 全局 | README 仍在描述旧版 `/iptv/<url>` 与 `/proxy/host:port/path` 双路由模式，而代码早已改为统一的 `?url=` 模式。`deploy-openwrt.sh`(L139)、`test-performance.sh`(L34,38)、CI 工作流(L88) 中的测试 URL **全部是旧格式，必然失败**。新人按 README 操作会完全跑不通。 |

#### 🟠 P1 —— 功能正确性缺陷

| ID | 问题 | 位置 | 说明 |
|----|------|------|------|
| **P1-1** | **`response_body_filter` 未处理分块边界** | L274-353 | Pingora 以流式 chunk 回调 body，代码对**每个 chunk 独立**做 `from_utf8` + 逐行解析。若 m3u8 恰好在 chunk 边界被切断（例如一行 URL 被拆成两半），会产生两条畸形行，导致改写失败或播放列表损坏；非 UTF-8 边界还会直接跳过改写。**正确做法是跨 chunk 累积缓冲，在 `_end_of_stream==true` 时一次性处理。** |
| **P1-2** | **标签内嵌 URI 完全未改写** | L285-343 | 只处理"独立成行的 URI"，但 HLS 中大量 URI 藏在标签的 `URI="..."` 属性里：`#EXT-X-KEY:METHOD=AES-128,URI="key.key"`、`#EXT-X-MAP:URI="init.mp4"`、`#EXT-X-MEDIA:URI=...`、`#EXT-X-I-FRAME-STREAM-INF:URI=...`。**AES-128 加密流（`#EXT-X-KEY`）与 fMP4 流的 init 段将无法播放**。`.key` 虽已列入 `MEDIA_EXTS`，但因永不作为独立行出现，实际是死代码。 |
| **P1-3** | **纯数字文件名被误判为非媒体资源** | L77-79 | `if filename.is_empty() \|\| filename.chars().all(is_ascii_digit) { return false }`。而 IPTV 直播流极常用 `00001.ts` / `12345.ts` 这类纯数字序号分片 —— **这类分片将不被改写，播放器会直连源站，很可能因 Referer/IP 限制而失败**。这是最可能"现场翻车"的一条规则。 |
| **P1-4** | **m3u8 判定依赖 URL 后缀** | L130 | `url.path().ends_with(".m3u8")` 才改写。若源站用 `/playlist?id=xxx` 或 `/live/stream` 这类无后缀地址（十分常见），即使 `Content-Type: application/vnd.apple.mpegurl` 也不会被改写。**建议补充：响应头 Content-Type 判定 + 正文以 `#EXTM3U` 开头判定。** |
| **P1-5** | **`.ts → .jpeg` 全局替换** | L187 | `path_and_query.replace(".ts", ".jpeg")` 会替换路径/查询串中**所有** `.ts` 子串。若签名 token 或路径中恰好含 `.ts`（如 `.../a.tsign=...`），会被破坏。应只替换最后一个路径段的扩展名。 |

#### 🟡 P2 —— 工程规范与可维护性

| ID | 问题 | 说明 |
|----|------|------|
| **P2-1** | **7 个未使用依赖** | `regex`/`once_cell`/`ahash`/`memchr`/`num_cpus`/`reqwest`/`tokio` 均未使用，README 宣称的 "SIMD 加速（memchr）" 并未实现。建议删除，可显著缩短编译时间。 |
| **P2-2** | **缺少 `Cargo.lock`** | `pingora` 以 Git rev 引入但未提交 lock 文件，构建**不可复现**且每次 CI 都可能拉到不同的传递依赖版本。二进制类项目应提交 `Cargo.lock`。 |
| **P2-3** | **`WORKERS` 环境变量形同虚设** | init 脚本与 README 都设置了 `WORKERS=4`，但 `main.rs` 从未读取它，也未调用 Pingora 的线程数配置接口。文档与实现不一致。 |
| **P2-4** | **`deploy.sh` 明文硬编码密码** | `deploy.sh` 中 `sshpass -p '123456tyui'` 及 sudo 密码明文写死；`deploy-openwrt.sh` 同样硬编码 `TARGET_PASS="password"`。**凭据已随代码入库，应立即轮换并从版本库中清除**（建议改用 SSH key + `ssh-agent`）。 |
| **P2-5** | **单文件 413 行，缺少测试** | 全部逻辑堆在 `main.rs`，无任何单元测试。m3u8 改写是纯字符串变换，非常适合表驱动测试（可覆盖纯数字文件名、相对/绝对路径、`.jpeg` 反转、`pending_tag` 配对等边界）。 |
| **P2-6** | **无超时与重试配置** | `HttpPeer` 未设置 connect/read 超时，也未实现 `fail_to_connect` 等错误处理钩子，上游异常时只能依赖 Pingora 默认的 502。 |
| **P2-7** | **`x-forwarded-for` 原样透传** | 直接转发客户端带来的 XFF，存在 IP 伪造风险；应按需追加而非透传。 |
| **P2-8** | **无请求方法限制** | 未限制 GET/HEAD，任意方法（含 POST）可被转发到任意 URL。 |

---

## 八、改进建议（按优先级排序）

1. **先修 P0-3（文档一致性）**：把 README、两个部署脚本、CI 工作流中的示例 URL 统一改成 `?url=` 格式 —— 成本最低、收益最直接。
2. **修 P1-1（缓冲式改写）**：在 `ProxyContext` 中增加一个 `body_buffer: Vec<u8>`，chunk 到达时追加，仅当 `_end_of_stream` 为 true 时整体改写并替换 `*body`。这是正确性的根本保障。
3. **修 P0-2（去掉 `expect`）**：改为优雅降级，或至少把 `panic="abort"` 改回 `unwind` 以隔离故障。
4. **修 P1-2（标签内 URI 改写）**：增加对 `#EXT-X-KEY` / `#EXT-X-MAP` / `#EXT-X-MEDIA` / `#EXT-X-I-FRAME-STREAM-INF` 的 `URI="..."` 属性解析与改写，并将相对 URI 按 `base_url` 补全。
5. **重新评估 P1-3（纯数字文件名规则）**：建议删除该排除规则，或改为"仅当整行是纯数字（不含扩展名）时才排除"。
6. **修 P1-4（m3u8 判定）**：增加 `Content-Type` 与 `#EXTM3U` 前缀双重兜底判定。
7. **安全加固**：加白名单或 token 鉴权（P0-1）；轮换并移除硬编码密码（P2-4）。
8. **工程化**：删除 7 个死依赖、提交 `Cargo.lock`、拆分 `m3u8.rs` 改写模块并补充单元测试。

---

## 九、总结

**这是一个"小而锐利"的工程**：用 413 行 Rust 解决了一个非常具体的现实问题（内网播放外部 IPTV 源），核心的 m3u8 递归改写 + 请求头伪造 + 重定向回包三件套设计得相当实用，部署链路（musl 静态编译 → procd → sysctl 调优）也完整闭环。

**但它的成熟度停留在"能跑通主要场景"的原型阶段**，存在三类系统性短板：

1. **文档与实现脱节**（README、部署脚本、CI 测试 URL 全部是旧版路由格式，照抄必失败）；
2. **流式处理与 HLS 协议覆盖不完整**（chunk 边界未处理、标签内 URI 未改写、纯数字分片被误判），这些会在特定源站上表现为"偶发播放失败"，排查成本极高；
3. **安全与工程规范缺失**（无访问限制的 SSRF 入口、`panic=abort` 下的进程崩溃风险、明文凭据入库、无 lock 文件、无测试、7 个死依赖）。

建议按第八节的优先级顺序推进，其中 **第 1、2、3 项应在下一次上线前完成**。

---

*报告结束*
