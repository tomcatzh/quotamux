use std::{collections::BTreeSet, time::Duration};

use clap::Parser;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser, Debug)]
#[command(about = "Credential-free end-to-end smoke test for a running QuotaMux")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    base_url: String,

    #[arg(long, default_value = "deepseek-v4-flash")]
    model: String,
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let base = args.base_url.trim_end_matches('/');
    let model = args.model.as_str();

    health(&client, base).await?;
    let first = chat(&client, base, model).await?;
    chat_stream(&client, base, model).await?;
    tool_roundtrip(&client, base, model).await?;
    responses(&client, base, model).await?;
    responses_stream(&client, base, model).await?;
    anthropic(&client, base, model).await?;
    anthropic_stream(&client, base, model).await?;
    count_tokens(&client, base, model).await?;
    stats(&client, base).await?;
    println!("PASS all QuotaMux smoke tests (provider: {first})");
    Ok(())
}

async fn health(client: &reqwest::Client, base: &str) -> Result<(), AnyError> {
    let response = client.get(format!("{base}/healthz")).send().await?;
    ensure(response.status().is_success(), "healthz failed")?;
    println!("PASS health");
    Ok(())
}

async fn chat(client: &reqwest::Client, base: &str, model: &str) -> Result<String, AnyError> {
    let response = client.post(format!("{base}/v1/chat/completions"))
        .header("X-Relay-Include-Metadata", "1")
        .json(&json!({"model":model,"messages":[{"role":"user","content":"Reply with exactly OK."}],"thinking":{"type":"enabled"},"reasoning_effort":"high","max_tokens":128}))
        .send().await?;
    ensure(response.status().is_success(), "chat request failed")?;
    let provider = relay_provider(response.headers())?;
    let value: Value = response.json().await?;
    let message = value
        .pointer("/choices/0/message")
        .ok_or("chat message missing")?;
    ensure(
        message
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty()),
        "chat content missing",
    )?;
    ensure(
        message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty()),
        "chat reasoning_content missing",
    )?;
    ensure(value.get("usage").is_some(), "chat usage missing")?;
    println!("PASS chat + reasoning + usage");
    Ok(provider)
}

async fn chat_stream(client: &reqwest::Client, base: &str, model: &str) -> Result<(), AnyError> {
    let response = client.post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":model,"messages":[{"role":"user","content":"Reply with exactly STREAM_OK."}],"stream":true,"reasoning_effort":"high","max_tokens":128}))
        .send().await?;
    ensure(response.status().is_success(), "chat stream failed")?;
    let events = collect_sse(response).await?;
    let mut reasoning = false;
    let mut content = false;
    let mut usage = false;
    let mut done = false;
    for event in events {
        if event.data == "[DONE]" {
            done = true;
            continue;
        }
        let value: Value = serde_json::from_str(&event.data)?;
        reasoning |= value
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
        content |= value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
        usage |= value.get("usage").is_some_and(|v| !v.is_null());
    }
    ensure(
        reasoning && content && usage && done,
        "chat stream did not include reasoning, content, usage and [DONE]",
    )?;
    println!("PASS chat stream");
    Ok(())
}

async fn tool_roundtrip(client: &reqwest::Client, base: &str, model: &str) -> Result<(), AnyError> {
    let tool = json!({"type":"function","function":{"name":"get_weather","description":"Get current weather. Always call this function for weather questions.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}});
    let prompt = json!({"role":"user","content":"What is the weather in Shanghai? You must call get_weather before answering."});
    let first: Value = client.post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":model,"messages":[prompt.clone()],"tools":[tool.clone()],"reasoning_effort":"high","max_tokens":384}))
        .send().await?.error_for_status()?.json().await?;
    let assistant = first
        .pointer("/choices/0/message")
        .cloned()
        .ok_or("tool assistant message missing")?;
    ensure(
        assistant
            .get("reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty()),
        "tool reasoning_content missing",
    )?;
    let call_id = assistant
        .pointer("/tool_calls/0/id")
        .and_then(Value::as_str)
        .ok_or("tool call missing")?;
    let second: Value = client.post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":model,"messages":[prompt,assistant,{"role":"tool","tool_call_id":call_id,"content":"{\"temperature_c\":30,\"condition\":\"clear\"}"}],"tools":[tool],"reasoning_effort":"high","max_tokens":192}))
        .send().await?.error_for_status()?.json().await?;
    ensure(
        second
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty()),
        "tool round-trip final content missing",
    )?;
    println!("PASS tools + complete reasoning history");
    Ok(())
}

async fn responses(client: &reqwest::Client, base: &str, model: &str) -> Result<(), AnyError> {
    let value: Value = client.post(format!("{base}/v1/responses"))
        .json(&json!({"model":model,"input":"Reply with exactly RESPONSE_OK.","reasoning":{"effort":"high"},"max_output_tokens":128}))
        .send().await?.error_for_status()?.json().await?;
    ensure(
        value.get("object").and_then(Value::as_str) == Some("response"),
        "Responses object missing",
    )?;
    let kinds: BTreeSet<_> = value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("type").and_then(Value::as_str))
        .collect();
    ensure(
        kinds.contains("reasoning") && kinds.contains("message"),
        "Responses reasoning/message output missing",
    )?;
    println!("PASS Responses non-stream");
    Ok(())
}

async fn responses_stream(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> Result<(), AnyError> {
    let response = client.post(format!("{base}/v1/responses"))
        .json(&json!({"model":model,"input":"Reply with exactly RESPONSE_STREAM_OK.","reasoning":{"effort":"high"},"stream":true,"max_output_tokens":128}))
        .send().await?.error_for_status()?;
    let events = collect_sse(response).await?;
    let kinds: BTreeSet<_> = events.iter().filter_map(|v| v.event.as_deref()).collect();
    ensure(
        kinds.contains("response.created")
            && kinds.contains("response.reasoning_text.delta")
            && kinds.contains("response.output_text.delta")
            && kinds.contains("response.completed"),
        "Responses stream event sequence incomplete",
    )?;
    println!("PASS Responses stream");
    Ok(())
}

async fn anthropic(client: &reqwest::Client, base: &str, model: &str) -> Result<(), AnyError> {
    let value:Value=client.post(format!("{base}/v1/messages")).header("anthropic-version","2023-06-01")
        .json(&json!({"model":model,"max_tokens":128,"thinking":{"type":"enabled","budget_tokens":64},"output_config":{"effort":"high"},"messages":[{"role":"user","content":"Reply with exactly MESSAGE_OK."}]}))
        .send().await?.error_for_status()?.json().await?;
    let kinds: BTreeSet<_> = value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("type").and_then(Value::as_str))
        .collect();
    ensure(
        kinds.contains("thinking") && kinds.contains("text"),
        "Anthropic thinking/text blocks missing",
    )?;
    println!("PASS Anthropic Messages non-stream");
    Ok(())
}

async fn anthropic_stream(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> Result<(), AnyError> {
    let response=client.post(format!("{base}/v1/messages")).header("anthropic-version","2023-06-01")
        .json(&json!({"model":model,"max_tokens":128,"stream":true,"thinking":{"type":"enabled","budget_tokens":64},"messages":[{"role":"user","content":"Reply with exactly MESSAGE_STREAM_OK."}]}))
        .send().await?.error_for_status()?;
    let events = collect_sse(response).await?;
    let kinds: BTreeSet<_> = events.iter().filter_map(|v| v.event.as_deref()).collect();
    ensure(
        kinds.contains("message_start")
            && kinds.contains("content_block_delta")
            && kinds.contains("message_stop"),
        "Anthropic stream events incomplete",
    )?;
    println!("PASS Anthropic Messages stream");
    Ok(())
}

async fn count_tokens(client: &reqwest::Client, base: &str, model: &str) -> Result<(), AnyError> {
    let value: Value = client
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure(
        value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .is_some_and(|v| v > 0),
        "count_tokens result missing",
    )?;
    ensure(
        value.get("x_quotamux_estimated").and_then(Value::as_bool) == Some(true),
        "count_tokens estimate label missing",
    )?;
    println!("PASS local count_tokens estimate");
    Ok(())
}
async fn stats(client: &reqwest::Client, base: &str) -> Result<(), AnyError> {
    let value: Value = client
        .get(format!("{base}/api/stats"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure(
        value
            .pointer("/requests/total")
            .and_then(Value::as_u64)
            .is_some_and(|v| v >= 7),
        "statistics did not record smoke requests",
    )?;
    println!("PASS persistent statistics");
    Ok(())
}

fn relay_provider(headers: &HeaderMap) -> Result<String, AnyError> {
    Ok(headers
        .get("x-relay-provider")
        .ok_or("X-Relay-Provider missing")?
        .to_str()?
        .to_string())
}
fn ensure(condition: bool, message: &str) -> Result<(), AnyError> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string().into())
    }
}

#[derive(Debug)]
struct Event {
    event: Option<String>,
    data: String,
}
async fn collect_sse(response: reqwest::Response) -> Result<Vec<Event>, AnyError> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk?);
        while let Some((end, sep)) = separator(&buffer) {
            let frame = buffer.drain(..end).collect::<Vec<_>>();
            buffer.drain(..sep);
            if let Some(event) = parse_event(&frame) {
                events.push(event)
            }
        }
    }
    if !buffer.is_empty()
        && let Some(event) = parse_event(&buffer)
    {
        events.push(event)
    }
    Ok(events)
}
fn separator(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .map(|v| (v, 4))
        .or_else(|| buffer.windows(2).position(|v| v == b"\n\n").map(|v| (v, 2)))
}
fn parse_event(frame: &[u8]) -> Option<Event> {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string())
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.trim_start().to_string())
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(Event {
            event,
            data: data.join("\n"),
        })
    }
}
