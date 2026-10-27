//! 真机探针：格式化转义 + 编辑频率（429 阈值）。
//!
//! 用法：
//!   tg_probe fmt  <chat_id>
//!   tg_probe rate <chat_id>

use std::time::{Duration, Instant};

use harness_channels::html::to_html;
use harness_channels::telegram::api::Api;
use harness_channels::telegram::http::HttpApi;
use harness_kernel::MsgRef;

fn token() -> String {
    std::fs::read_to_string(
        std::env::var("HOME").unwrap() + "/.config/chuan/secrets/telegram_token",
    )
    .expect("token file")
    .trim()
    .to_owned()
}

fn cases() -> Vec<(&'static str, String)> {
    vec![
        ("html 特殊字符", "普通 <tag> & 符号 > 结束".to_string()),
        ("粗斜体与行内码", "**粗体** 和 *斜体* 和 `行内 <code>`".to_string()),
        ("代码围栏", "```python\nprint('a < b & c')\n```".to_string()),
        ("未闭合围栏", "```\n半截代码".to_string()),
        ("链接带 &", "[文档](https://example.com/?a=1&b=2)".to_string()),
        ("注入尝试", "正常 </b> <script>alert(1)</script> <a href='x'>".to_string()),
        ("嵌套", "**粗里有 `code` 还有 *斜* **".to_string()),
        ("MarkdownV2 雷区", "a_b*c[d]e(f).!#+-=|{}~>#".to_string()),
        ("表情与 CJK", "👨‍👩‍👧 家庭 **重点** & 完成".to_string()),
        ("长文本", "长".repeat(3000)),
        ("空行与列表", "第一行\n\n- 项目一\n- 项目二\n\n结尾".to_string()),
    ]
}

async fn fmt(api: &HttpApi, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    for (name, raw) in cases() {
        let html = to_html(&raw);
        let shown = if html.chars().count() > 160 {
            format!("{}…", html.chars().take(160).collect::<String>())
        } else {
            html.clone()
        };
        match api.send_message(chat, &html, true).await {
            Ok(_) => println!("[ OK ] {name:<16} len={:<5} {shown}", html.chars().count()),
            Err(e) => println!("[FAIL] {name:<16} {e}\n       html={shown}"),
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    Ok(())
}

async fn rate(api: &HttpApi, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    let msg: MsgRef = api.send_message(chat, "rate test", true).await?;

    let mut fails = 0;
    for i in 0..20 {
        let t = format!("rate A {i}");
        match api.edit_message(chat, &msg, &t, true).await {
            Ok(_) => {}
            Err(e) => {
                fails += 1;
                println!("  A#{i} 失败: {e}");
            }
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    println!("阶段 A（1000ms 间隔 ×20）：失败 {fails} 次");

    let start = Instant::now();
    let mut first_429 = None;
    for i in 0..40 {
        let t = format!("rate B {i}");
        match api.edit_message(chat, &msg, &t, true).await {
            Ok(_) => {}
            Err(e) => {
                first_429 = Some((i, e.to_string()));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("阶段 B（100ms 间隔）：首次失败 {:?}，用时 {:?}", first_429, start.elapsed());
    Ok(())
}

async fn verify(api: &HttpApi, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        ("嵌套", "**粗里有 `code` 还有 *斜***"),
        ("代码围栏", "```python\nprint('a < b & c')\n```"),
        ("注入尝试", "正常 </b> <script>x</script>"),
        ("链接", "[文档](https://example.com/?a=1&b=2)"),
        ("纯文本", "a_b*c[d]e(f).!#+-=|{}~>#"),
        ("三星号粗斜", "***重点***"),
        ("表格", "| 步骤 | 操作 | 结果 |\n|---|---|---|\n| 1 | 写入 hello.txt | 成功 |\n| 2 | 读取文件 | 内容为 hi |"),
    ];
    for (name, raw) in cases {
        let html = to_html(raw);
        match api.send_message_verbose(chat, &html).await {
            Ok(v) => {
                let text = v["text"].as_str().unwrap_or_default();
                let types: Vec<String> = v["entities"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|e| e["type"].as_str().unwrap_or("?").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                println!(
                    "{name:<10} text={:?}\n           entities={types:?}",
                    text.chars().take(48).collect::<String>()
                );
            }
            Err(e) => println!("[FAIL] {name}: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    Ok(())
}

async fn burst(api: &HttpApi, chat: i64) -> Result<(), Box<dyn std::error::Error>> {
    let msg: MsgRef = api.send_message(chat, "burst test", true).await?;
    let api = std::sync::Arc::new(HttpApi::new(token())?);
    let mut tasks = Vec::new();
    let start = Instant::now();
    for i in 0..30 {
        let api = std::sync::Arc::clone(&api);
        let msg = msg.clone();
        tasks.push(tokio::spawn(async move {
            let t = format!("burst {i}");
            (i, api.edit_message(chat, &msg, &t, true).await)
        }));
    }
    let mut ok = 0;
    let mut retry: Option<String> = None;
    for t in tasks {
        let (i, r) = t.await?;
        match r {
            Ok(_) => ok += 1,
            Err(e) => {
                println!("  #{i} 失败: {e}");
                retry.get_or_insert_with(|| e.to_string());
            }
        }
    }
    println!("并发 30 次：成功 {ok}，首错 {retry:?}，耗时 {:?}", start.elapsed());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let chat: i64 = args.next().unwrap_or_default().parse()?;
    let api = HttpApi::new(token())?;
    match mode.as_str() {
        "fmt" => fmt(&api, chat).await,
        "rate" => rate(&api, chat).await,
        "burst" => burst(&api, chat).await,
        "verify" => verify(&api, chat).await,
        _ => {
            eprintln!("用法: tg_probe fmt|verify|rate|burst <chat_id>");
            Ok(())
        }
    }
}
