//! 模型窗口/能力的匹配策略（D43）：表只提供数据，这里决定"对不上怎么退"。
//!
//! 四级，从严到宽：
//! 1. **provider + model 精确命中** —— 最好情况，`(zai, glm-5.3-flash)`；
//! 2. **provider 命中、model 带版本尾巴** —— `deepseek-v4.1-flash-1017` 归到同家的 `deepseek-v4.1-flash`；
//! 3. **同名模型跨 provider 取均值** —— 用官方/中转/聚合站都可能，provider 名对不上时的主力路径；
//! 4. **同名去版本后取均值** —— 连版本尾巴都不一样（`-latest` / 日期）时兜底。
//!
//! 再对不上就返回 `None`，由调用方退回保守预算并**明说认不出**——绝不猜一个大窗口。
//!
//! 为什么第 3/4 级取**均值**而不是最小：同一模型的标称窗口通常一致，个别 provider 标得低是
//! 因为它自己限制了；取最小会把"其实能用 1M"的环境按 128k 跑。均值更接近真实，
//! 而真被某家限制顶到 400 时，`[engine] context_window` 可以一行覆盖。
//!
//! 覆盖优先级（在 `config.rs` 里）：显式 `context_window` / `context_budget_tokens` > 本模块。

use crate::model_window::{Info, ROWS};

/// 命中方式（给人看，也用于启动行说明"这个窗口是哪来的"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// provider + model 精确
    Exact,
    /// 同 provider、model 去版本后命中
    SameProvider,
    /// 同名模型跨 provider 均值
    ModelAvg,
    /// 去版本后同名跨 provider 均值
    StemAvg,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Exact => "精确匹配",
            Tier::SameProvider => "同家去版本",
            Tier::ModelAvg => "同名均值",
            Tier::StemAvg => "同名去版本均值",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picked {
    pub info: Info,
    pub tier: Tier,
    /// 参与聚合的条数（1 = 单条命中）
    pub samples: usize,
}

impl Picked {
    /// 启动行用的说明：`同名均值(3 家)`。
    pub fn describe(&self) -> String {
        if self.samples <= 1 {
            self.tier.label().to_owned()
        } else {
            format!("{}({} 家)", self.tier.label(), self.samples)
        }
    }
}

/// provider 别名：配置里可能写的是厂商名/产品名，而 models.dev 用的是它的 provider id。
/// 别名只影响"查表"，不影响请求发往哪（base_url 仍由 preset / 配置决定）。
/// 只收"配置里可能写、但表里没有同 id"的名字。表里有的 id（opencode / google-vertex /
/// tencent-coding-plan …）**不要写成别名**——那会把精确命中降级成均值。
const ALIASES: &[(&str, &str)] = &[
    ("zhipu", "zai"),
    ("zhipuai", "zai"),
    ("bigmodel", "zai"),
    ("dashscope", "alibaba"),
    ("bailian", "alibaba"),
    ("aliyun", "alibaba"),
    ("qwen", "alibaba"),
    ("kimi", "moonshotai"),
    ("moonshot", "moonshotai"),
    ("tencent", "tencent-coding-plan"),
    ("hunyuan", "tencent-coding-plan"),
    ("ark", "volcengine"),
    ("doubao", "volcengine"),
    ("mimo", "xiaomi"),
];

/// model 名里那些"不是型号"的尾巴：版本日期与常见标签。
/// 注意只丢**尾部连续**的这些词，`gpt-4.1`、`glm-5.3` 这种型号内的点/数字不受影响。
/// `max` / `turbo` / `flash` / `plus` 这类是**型号的一部分**，不要放进来：
/// 削掉就等于把 `qwen3.5-max` 认成 `qwen3.5`，窗口和模态都会串。
const TAGS: &[&str] = &[
    "latest", "free", "preview", "exp", "experimental", "thinking", "instruct", "chat",
    "reasoning", "nonreasoning",
];

/// 把配置里的 provider 归一到 models.dev 的 id。未命中别名表就原样返回。
fn norm_provider(provider: &str) -> String {
    let p = provider.trim().to_ascii_lowercase();
    ALIASES
        .iter()
        .find(|(k, _)| *k == p)
        .map(|(_, v)| (*v).to_owned())
        .unwrap_or(p)
}

/// 去掉 `vendor/`、`:variant`，只留模型名本体。
fn norm_model(model: &str) -> String {
    let s = model.trim().to_ascii_lowercase();
    let s = s.rsplit('/').next().unwrap_or(&s);
    let s = s.split(':').next().unwrap_or(s);
    s.to_owned()
}

/// 去掉尾部的版本日期 / 标签，得到"型号主干"。`deepseek-v4.1-flash-1017` → `deepseek-v4.1-flash`。
///
/// 只丢连续尾部：`glm-5-turbo` 里的 `turbo` 是型号的一部分，不在 `TAGS` 里，不会被动。
fn stem(model: &str) -> String {
    let mut parts: Vec<&str> = model.split('-').collect();
    while let Some(last) = parts.last() {
        let is_date = last.len() >= 4 && last.len() <= 8 && last.chars().all(|c| c.is_ascii_digit());
        let is_tag = TAGS.contains(last);
        if is_date || is_tag {
            parts.pop();
        } else {
            break;
        }
    }
    parts.join("-")
}

/// 主入口：返回窗口/输出/模态与命中方式。
pub fn pick(provider: &str, model: &str) -> Option<Picked> {
    let want_p = norm_provider(provider);
    let want_m = norm_model(model);
    if want_m.is_empty() {
        return None;
    }
    let want_stem = stem(&want_m);

    // 1. provider + model 精确
    if let Some(row) = ROWS
        .iter()
        .find(|(p, m, ..)| *p == want_p && norm_model(m) == want_m)
    {
        return Some(Picked { info: info(row), tier: Tier::Exact, samples: 1 });
    }
    // 2. 同 provider，model 去版本
    if let Some(row) = ROWS
        .iter()
        .find(|(p, m, ..)| *p == want_p && stem(&norm_model(m)) == want_stem)
    {
        return Some(Picked { info: info(row), tier: Tier::SameProvider, samples: 1 });
    }
    // 3. 同名跨 provider 取均值
    if let Some((info, n)) = aggregate(|m| norm_model(m) == want_m) {
        return Some(Picked { info, tier: Tier::ModelAvg, samples: n });
    }
    // 4. 去版本同名跨 provider 取均值
    if let Some((info, n)) = aggregate(|m| stem(&norm_model(m)) == want_stem) {
        return Some(Picked { info, tier: Tier::StemAvg, samples: n });
    }
    None
}

fn info(row: &(&'static str, &'static str, u64, u64, u8)) -> Info {
    Info { context: row.2, output: row.3, input: row.4 }
}

/// 多家同名模型取均值（向下取整），模态取并集。
///
/// 窗口取均值而不是最小：同一模型的标称值通常一致，个别 provider 标得低是它自己的限制；
/// 取最小会把"其实能用 1M"的环境按 128k 跑。真被某家顶到 400 时用 `context_window` 覆盖。
fn aggregate(match_fn: impl Fn(&str) -> bool) -> Option<(Info, usize)> {
    let mut n = 0u64;
    let mut ctx = 0u64;
    let mut out = 0u64;
    let mut input = 0u8;
    for (_, m, c, o, mask) in ROWS {
        if !match_fn(m) {
            continue;
        }
        n += 1;
        ctx += *c;
        out += *o;
        input |= *mask;
    }
    if n == 0 {
        return None;
    }
    let context = ctx / n;
    Some((Info { context, output: (out / n).min(context), input }, n as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_provider_wins() {
        let p = pick("opencode-go", "deepseek-v4.1-flash").unwrap();
        assert_eq!(p.tier, Tier::Exact);
        assert_eq!(p.info.context, 1_000_000);
        assert!(p.info.supports_image());
    }

    #[test]
    fn provider_alias_maps_to_table_id() {
        // 配置里写 zhipuai / opencode，也能命中 zai / opencode-go
        let a = pick("zhipuai", "glm-5.3-flash").unwrap();
        let b = pick("zai", "glm-5.3-flash").unwrap();
        assert_eq!(a.info, b.info);
        assert_eq!(a.tier, Tier::Exact);
    }

    #[test]
    fn vendor_prefix_and_variant_suffix_ignored() {
        // 中转站常见的写法：vendor/ 前缀、@ 或 : 变体
        let p = pick("some-relay", "deepseek/deepseek-v4.1-flash:free").unwrap();
        // 各家标称 1M / 1 048 576 都算同一档，均值落在中间
        assert!((1_000_000..=1_100_000).contains(&p.info.context), "{}", p.info.context);
        assert!(p.info.supports_image());
    }

    #[test]
    fn version_tail_matches_same_family() {
        // 同一家：带日期的 id 归到不带日期的那条（SameProvider）
        let p = pick("anthropic", "claude-haiku-4-5-20260101").unwrap();
        assert_eq!(p.tier, Tier::SameProvider);
        assert_eq!(p.info.context, 200_000);

        // 换一家不认识的 provider：退回"同名/同主干"均值
        let p = pick("no-such-provider", "claude-haiku-4-5-20260101").unwrap();
        assert!(matches!(p.tier, Tier::ModelAvg | Tier::StemAvg), "{:?}", p.tier);
        assert_eq!(p.info.context, 200_000);
    }

    /// provider 对不上时的主力路径：同名模型跨 provider 取均值。
    #[test]
    fn unknown_provider_falls_back_to_model_average() {
        let p = pick("my-own-relay", "glm-5.3-flash").unwrap();
        assert_eq!(p.tier, Tier::ModelAvg);
        assert!(p.samples >= 2, "应该聚合了多家：{}", p.samples);
        // 各家标称 1M 上下（有的按 2^20 记），均值仍在 1M 档
        assert!((1_000_000..=1_100_000).contains(&p.info.context), "{}", p.info.context);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(pick("zai", "totally-made-up-model"), None);
        assert_eq!(pick("zai", ""), None);
    }

    /// 型号里的 `turbo`/`v4.1` 不能被当成版本尾巴削掉。
    #[test]
    fn stem_keeps_model_name_intact() {
        assert_eq!(stem("glm-5-turbo"), "glm-5-turbo");
        assert_eq!(stem("glm-5.3-flash"), "glm-5.3-flash");
        assert_eq!(stem("deepseek-v4.1-flash-1017"), "deepseek-v4.1-flash");
        assert_eq!(stem("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(stem("qwen3.5-max-latest"), "qwen3.5-max");
    }
}
