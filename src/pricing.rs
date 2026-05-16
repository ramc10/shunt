/// Anthropic / OpenAI API pricing table and cost helpers.
///
/// Prices are charged per million tokens. We use the public list prices as
/// of 2025 — close enough for "what would this have cost on the API?" display.

/// Cost in USD if the given tokens had been sent through the public API.
pub fn api_cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let (input_price, output_price) = model_prices(model);
    (input_tokens  as f64 / 1_000_000.0) * input_price
        + (output_tokens as f64 / 1_000_000.0) * output_price
}

/// (input_price_per_mtok, output_price_per_mtok) in USD.
fn model_prices(model: &str) -> (f64, f64) {
    if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("haiku") {
        (0.80, 4.0)
    } else if model.contains("sonnet") || model.is_empty() {
        (3.0, 15.0)
    } else if model.starts_with("gpt-4") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("gpt-5") {
        // GPT-4o pricing as a proxy for OpenAI models
        (5.0, 15.0)
    } else {
        // Unknown — default to Sonnet pricing
        (3.0, 15.0)
    }
}

/// Format a dollar amount compactly: "$0.04", "$1.23", "$840", "$4.2k".
pub fn fmt_cost(usd: f64) -> String {
    if usd >= 10_000.0 {
        format!("${:.0}k", usd / 1_000.0)
    } else if usd >= 1_000.0 {
        format!("${:.1}k", usd / 1_000.0)
    } else if usd >= 1.0 {
        format!("${:.2}", usd)
    } else if usd >= 0.01 {
        format!("${:.2}", usd)
    } else if usd > 0.0 {
        "<$0.01".to_owned()
    } else {
        "$0".to_owned()
    }
}
