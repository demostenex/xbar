use std::path::PathBuf;

use xbar_ai_usage::{fetch_anthropic, fetch_openai, AccountIdentity, ProviderUsage, UsageValue};

fn print_usage(usage: ProviderUsage) {
    println!("provider={} status={:?}", usage.provider_id, usage.status);
    println!("display_name={}", usage.display_name);
    println!("meters={}", usage.meters.len());
    for meter in usage.meters {
        let value = match meter.value {
            Some(UsageValue::Percentage {
                used_pct,
                remaining_pct,
            }) => format!("percentage used={used_pct:?} remaining={remaining_pct:?}"),
            Some(UsageValue::Amount { value, unit }) => {
                format!("amount value={value} unit={unit:?}")
            }
            Some(UsageValue::Count { value, unit }) => {
                format!("count value={value} unit={unit:?}")
            }
            Some(UsageValue::Text { value, unit }) => {
                format!("text value={value} unit={unit:?}")
            }
            None => "none".into(),
        };
        println!(
            "  meter={} label={} reset_at={:?} {}",
            meter.id, meter.label, meter.reset_at, value
        );
    }
    println!(
        "summary primary_meter_id={:?} remaining_pct={:?}",
        usage.summary.primary_meter_id, usage.summary.remaining_pct
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is required");
    let codex = fetch_openai(
        &home.join(".codex/auth.json"),
        &home.join(".cache/xbar-ai-usage/openai"),
        AccountIdentity::Default,
    )
    .await;
    let claude = fetch_anthropic(
        &home.join(".claude/.credentials.json"),
        &home.join(".cache/xbar-ai-usage/anthropic"),
        AccountIdentity::Default,
    )
    .await;
    print_usage(codex);
    print_usage(claude);
}
