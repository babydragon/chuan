#!/usr/bin/env python3
"""从 models.dev 生成内置模型表（窗口 / 输出上限 / 输入模态）。

为什么离线生成、运行期不联网：
    不变量 9 规定出网只有两条路（宿主 `web_fetch` 代取 / 技能声明 `network`），
    启动时偷偷请求 models.dev 会绕过审计。表是快照，过期就重跑本脚本。

用法:
    python3 scripts/gen-model-windows.py                 # 联网拉 api.json 并写文件
    python3 scripts/gen-model-windows.py --input api.json # 用本地快照
    python3 scripts/gen-model-windows.py --providers zai,opencode-go

输出: apps/home/src/model_window.rs（`--out` 可改）
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_OUT = REPO / "apps/home/src/model_window.rs"
API = "https://models.dev/api.json"

# 默认只收**第一方厂商**（含它们的订阅/编码计划入口）：窗口与模态这类事实以厂商为准。
#
# 刻意**不收**聚合站与中转站（openrouter / nano-gpt / llmgateway / vercel / kilo …）：
#   1. 它们列的绝大多数是别家的同一个模型（去掉 vendor/ 前缀后 90% 重复），
#      收进来只会把源文件撑大、把"均值"带偏；
#   2. 用它们的人本来就走"provider 对不上 → 同名模型均值"这条回退（`model_pick.rs`）。
# 同理不收 amazon-bedrock / azure：模型 id 带区域与命名空间前缀，匹配收益低。
# 要临时加：--providers a,b,c（会替换默认列表）。
DEFAULT_PROVIDERS = [
    # 中国厂商（各自一方，含订阅/编码计划这类同门不同入口的 id）
    "zai",
    "zai-coding-plan",
    "zhipuai",
    "zhipuai-coding-plan",
    "deepseek",
    "alibaba",
    "alibaba-cn",
    "alibaba-coding-plan",
    "alibaba-coding-plan-cn",
    "alibaba-token-plan",
    "moonshotai",
    "moonshotai-cn",
    "kimi-code-plan-global",
    "kimi-code-plan-cn",
    "minimax",
    "minimax-cn",
    "minimax-coding-plan",
    "minimax-cn-coding-plan",
    "stepfun",
    "stepfun-ai",
    "tencent-coding-plan",
    "volcengine",
    "volcengine-coding-plan",
    "xiaomi",
    # 海外厂商
    "openai",
    "anthropic",
    "google",
    "google-vertex",
    "xai",
    "mistral",
    "cohere",
    "groq",
    "cerebras",
    "deepinfra",
    "fireworks-ai",
    "perplexity",
    # 编码计划（同时是聚合面：同一个模型在这里也有一份，均值会用到）
    "opencode",
    "opencode-go",
]

MODALITY_BITS = {"text": 1, "image": 2, "video": 4, "audio": 8, "pdf": 16}


def load(path: str | None) -> dict:
    if path:
        return json.loads(Path(path).read_text())
    # models.dev 对无 UA 的请求直接 403；带上 UA（也照常走 https_proxy 环境变量）
    req = urllib.request.Request(API, headers={"User-Agent": "chuan-gen-model-windows/1"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:  # noqa: S310 (数据源固定)
            return json.loads(r.read())
    except Exception as e:  # noqa: BLE001
        raise SystemExit(
            f"拉取 {API} 失败：{e}\n"
            "可以在浏览器里下载 api.json，然后用 --input 指定本地文件"
        ) from e


def rows(data: dict, providers: list[str]) -> list[tuple]:
    out = []
    for pid in providers:
        p = data.get(pid)
        if not p:
            print(f"warn: models.dev 里没有 provider {pid!r}，跳过", file=sys.stderr)
            continue
        for mid, m in p.get("models", {}).items():
            limit = m.get("limit") or {}
            ctx = int(limit.get("context") or 0)
            if ctx <= 0:
                continue
            # 只要**对话模型**：输出不含 text 的是画图/语音合成，读不懂提示词。
            outs = (m.get("modalities") or {}).get("output")
            if outs and "text" not in outs:
                continue
            # 窗口小到 4k 以下的是 embedding / whisper / prompt-guard 这类工具模型，
            # 不会有人拿它当 [engine] model；留着只会让"窗口可疑"的断言变成噪音。
            if ctx < 4096:
                continue
            # 有个别条目（TTS 之类）标的输出上限比窗口还大：按窗口收口，
            # 不然"预留输出"这类运算会算出不属于这个窗口的数。
            out_tokens = min(int(limit.get("output") or 0), ctx)
            out.append(
                (
                    pid,
                    mid.lower(),
                    ctx,
                    out_tokens,
                    modality_mask(m.get("modalities", {}).get("input", [])),
                )
            )
    out.sort(key=lambda r: (r[0], r[1]))
    return out


def modality_mask(inputs: list[str]) -> int:
    mask = 0
    for name in inputs:
        mask |= MODALITY_BITS.get(name, 0)
    return mask


def render(items: list[tuple], providers: list[str]) -> str:
    stamp = dt.date.today().isoformat()
    lines = [
        "//! 内置模型表：窗口 / 输出上限 / 输入模态。**由脚本生成，不要手改**。",
        "//!",
        f"//! 生成时间：{stamp}；数据源：models.dev（离线快照，运行期不联网）。",
        "//! 重新生成：`python3 scripts/gen-model-windows.py`",
        "//!",
        "//! 这里**只有数据**。provider 对不上时怎么回退、多家同名模型怎么取均值，",
        "//! 属于策略，在 `model_pick.rs` 里（改策略不需要重跑脚本）。",
        "//!",
        "//! 为什么不运行时拉：不变量 9（出网只有两条路且都留痕）。表过期就重跑脚本。",
        "",
        "/// 输入模态位掩码（与 models.dev 的 `modalities.input` 对应）。",
        "/// TEXT / PDF 目前没有调用方，但留着是为了让表自解释、以后加格式不必改口径。",
        "#[allow(dead_code)]",
        "pub const TEXT: u8 = 1;",
        "pub const IMAGE: u8 = 2;",
        "pub const VIDEO: u8 = 4;",
        "pub const AUDIO: u8 = 8;",
        "#[allow(dead_code)]",
        "pub const PDF: u8 = 16;",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct Info {",
        "    /// 上下文窗口（token）",
        "    pub context: u64,",
        "    /// 单次输出上限（token）",
        "    pub output: u64,",
        "    /// 输入模态位掩码",
        "    pub input: u8,",
        "}",
        "",
        "impl Info {",
        "    pub fn supports_image(self) -> bool {",
        "        self.input & IMAGE != 0",
        "    }",
        "",
        "    pub fn supports_video(self) -> bool {",
        "        self.input & VIDEO != 0",
        "    }",
        "",
        "    pub fn supports_audio(self) -> bool {",
        "        self.input & AUDIO != 0",
        "    }",
        "}",
        "",
        "/// (provider, model, 窗口, 输出上限, 输入模态)。provider / model 都是小写原样。",
        "pub const ROWS: &[(&str, &str, u64, u64, u8)] = &[",
    ]
    for pid, mid, ctx, out, mask in items:
        lines.append(f'    ("{pid}", "{mid}", {ctx}, {out}, {mask}),')
    lines += [
        "];",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::*;",
        "",
        "    #[test]",
        "    fn table_is_sane() {",
        "        // 空表说明生成脚本跑挂了：宁可测试红，也不要静默退回默认预算",
        "        assert!(ROWS.len() > 50, \"内置表太小（{} 条）\", ROWS.len());",
        "        for (p, m, ctx, out, _) in ROWS {",
        "            assert!(!p.is_empty() && !m.is_empty());",
        "            assert!(*ctx >= 4_096, \"{p}/{m} 窗口可疑：{ctx}\");",
        "            assert!(*out <= *ctx, \"{p}/{m} 输出上限大于窗口\");",
        "            assert_eq!(*p, p.to_ascii_lowercase(), \"{p} 应是小写\");",
        "        }",
        "    }",
        "}",
        "",
    ]
    return "\n".join(lines)
def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", help="本地 api.json（不联网）")
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--providers", default=",".join(DEFAULT_PROVIDERS))
    args = ap.parse_args()

    providers = [p.strip() for p in args.providers.split(",") if p.strip()]
    items = rows(load(args.input), providers)
    if not items:
        print("生成失败：一条模型都没取到", file=sys.stderr)
        return 1

    out = Path(args.out)
    out.write_text(render(items, providers))
    print(f"写入 {out}：{len(items)} 条模型 / {len(providers)} 个提供方")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
