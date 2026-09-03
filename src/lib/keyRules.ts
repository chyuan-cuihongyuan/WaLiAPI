/**
 * 网关 API 密钥的 allowed/denied 规则判定（C11 预重构：UsagePage /
 * AppConfigPage 各自内联的同一对守卫闭包收敛为单一实现）。
 */
import type { ApiKey } from "../types";

export interface KeyGuards {
  /** 渠道 id 是否被所选密钥允许（allowed_channels 白名单 + denied_channels 黑名单）。 */
  channelAllowed: (channelId: string) => boolean;
  /** 模型名是否被所选密钥允许（allowed_models 白名单 + denied_models 黑名单）。 */
  modelAllowed: (model: string) => boolean;
}

export function makeKeyGuards(key: ApiKey | undefined): KeyGuards {
  const channelAllowed = (channelId: string) => {
    if (!key) return true;
    if (key.allowed_channels.length > 0 && !key.allowed_channels.includes(channelId)) return false;
    if (key.denied_channels.includes(channelId)) return false;
    return true;
  };
  const modelAllowed = (model: string) => {
    if (!key) return true;
    if (key.allowed_models.length > 0 && !key.allowed_models.includes(model)) return false;
    if (key.denied_models.includes(model)) return false;
    return true;
  };
  return { channelAllowed, modelAllowed };
}
