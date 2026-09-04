# Codex 怎么去掉 API Key 模式（切回原账号）

> 适用场景：通过 WaLiAPI「应用配置 → Codex」一键写入网关配置后，想切回 ChatGPT 账号登录 / 官方 OpenAI 通道时使用。

---

## 一、先搞清楚：WaLiAPI 改了什么

WaLiAPI 一键接入 Codex 时，**只改 `~/.codex/config.toml`，不动 `auth.json`**：

```toml
model_provider = "waliapi"      # ← 新增：把默认 provider 指向网关
model = "gpt-5-codex"           # ← 新增：默认模型改为网关侧模型

[model_providers.waliapi]       # ← 新增：网关 provider 定义
name = "WaLiAPI Gateway"
base_url = "http://127.0.0.1:8777/v1"
wire_api = "responses"
experimental_bearer_token = "sk-..."   # ← WaLiAPI 的 API Key
```

配套保护机制：

- 写入前自动备份原配置到 `~/.codex/config.toml.waliapi-backup`
- 如果写入前 `config.toml` 不存在，会留标记文件 `config.toml.waliapi-absent`
- 不会写 `auth.json` 的 `OPENAI_API_KEY`（避免 Codex 拿网关 Key 去 OpenAI 官网验证导致 401）

因此"切回去"本质是：**把 `config.toml` 恢复原样，`auth.json` 里的 ChatGPT 登录态一直都在**。

---

## 二、方法一：WaLiAPI 界面一键切回（推荐）

打开 WaLiAPI → **应用配置 → Codex → 点击「切回原账号」**。

行为说明：

| 写入前状态 | 恢复动作 |
|---|---|
| 原本有 `config.toml` | 从 `.waliapi-backup` 备份还原 |
| 原本没有 `config.toml` | 删除 WaLiAPI 写入的配置文件 |

恢复后**重启 Codex** 即回到原账号（`auth.json` 全程未被改动）。

**API Key 模式检测（v0.2.7+）**：切回时 WaLiAPI 会自动检查 `auth.json`，若发现它被其他工具改成 API Key 模式（`auth_mode: "apikey"` 或设置了 `OPENAI_API_KEY` 且无 ChatGPT 登录态），结果面板会提示，并可一键点击「**重置为 ChatGPT 登录**」：

- 原 `auth.json` 自动备份为 `~/.codex/auth.json.waliapi-backup`
- 重置为 `{"auth_mode": "chatgpt", "OPENAI_API_KEY": null}`
- 之后运行 `codex login` 完成 ChatGPT 授权即可

---

## 三、方法二：手动编辑 `config.toml`

如果一键恢复不可用（比如手动改过配置），直接编辑 `~/.codex/config.toml`：

1. **删掉整个 `[model_providers.waliapi]` 段**
2. **删掉或注释掉顶部的 `model_provider = "waliapi"`**（或改回 `model_provider = "openai"`）
3. 检查是否有以下字段，有则删除或改回：
   ```toml
   preferred_auth_method = "apikey"   # 删除，或改为 "chatgpt"
   forced_login_method = "api"        # 删除，或改为 "chatgpt"
   ```
4. 保存后重启 Codex

> 提示：也可以直接用备份文件还原：
> ```bash
> cp ~/.codex/config.toml.waliapi-backup ~/.codex/config.toml
> ```

---

## 四、方法三：彻底重置（官方推荐的迁移流程）

适用于 `auth.json` 状态混乱、或想完全从头走一遍 ChatGPT 登录的情况。来自 OpenAI Codex 官方文档 *Migrating to ChatGPT login from API key*：

```bash
# 1. 确认 Codex CLI 版本 ≥ 0.20.0
codex --version

# 2. 删除本地凭据文件（Windows 在 C:\Users\<用户名>\.codex\auth.json）
rm ~/.codex/auth.json

# 3. 重新登录
codex login
```

浏览器会打开 ChatGPT 授权页，完成登录即可。

**补充检查项：**

- **环境变量残留**：`echo $OPENAI_API_KEY`，如果有值且来自旧的手工配置，按需 `unset OPENAI_API_KEY` 或从 `~/.zshrc` 等 shell 配置中移除。注意：若该变量被其他工具依赖，不要盲目删除，用 `preferred_auth_method = "chatgpt"` 让 Codex 忽略它即可。
- **验证当前认证方式**：进入 Codex TUI 后输入 `/status`，或执行 `codex login status`，确认走 ChatGPT 账号而非 API Key。

---

## 五、鉴权优先级速查（理解"为什么没切回去"）

Codex 的认证来源按以下逻辑生效，切回失败通常是某一层还在强制 apikey：

| 层 | 配置位置 | 切回时的处理 |
|---|---|---|
| 环境变量 | `OPENAI_API_KEY` | `unset` 或保留+用下行压制 |
| 认证偏好 | `config.toml` → `preferred_auth_method = "apikey"` | 删除或改 `"chatgpt"` |
| 强制登录方式 | `config.toml` → `forced_login_method = "api"` | 删除或改 `"chatgpt"` |
| 自定义 provider | `config.toml` → `model_provider` / `[model_providers.*]` | 删 waliapi 段，`model_provider` 还原 |
| 本地凭据 | `~/.codex/auth.json` | 一般不用动；混乱时删除后 `codex login` |

规则：`preferred_auth_method = "chatgpt"`（默认值）时，只要有 ChatGPT 登录态就优先用它；仅存在 API Key 时才回退到 Key 模式。

---

## 六、常见问题

**Q1：点「切回原账号」提示"没有找到备份文件"？**
说明写入前没有留下备份（或备份被清理）。按方法二手动编辑 `config.toml` 即可，只需删掉 waliapi 相关段。

**Q2：切回后 Codex 仍报 401 / 走了网关？**
大概率是 `model_provider = "waliapi"` 没删干净，或 shell 里还有 `OPENAI_API_KEY` 指向网关 Key。按第五节的优先级表逐层排查。

**Q3：之前是 ChatGPT 登录，接入网关后会被登出吗？**
不会。WaLiAPI 不碰 `auth.json`，切回后原登录态直接可用。只有手动执行过 `codex logout` 或删过 `auth.json` 才需要重新 `codex login`。

**Q4：想同时保留两套配置随时切换？**
用 Codex 的 profile 机制：把网关配置写到 `~/.codex/waliapi.config.toml`，需要走网关时 `codex --profile waliapi`，默认启动仍是原账号。

---

## 参考资料

- OpenAI Codex 官方认证文档：https://developers.openai.com/codex/auth
- `openai/codex` 仓库 config 说明：https://github.com/openai/codex/blob/main/docs/config.md
- WaLiAPI 实现：`src-tauri/src/commands/app_config.rs`（`write_codex` / `restore_config` / `detect_codex_apikey_mode` / `reset_codex_auth`）
