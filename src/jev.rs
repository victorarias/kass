use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

pub const DEFAULT_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";
// Measured 0.29-0.38s for a small file (2026-09-26); 15s only trips on a stuck request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(1000),
    Duration::from_millis(3000),
];

pub struct Client {
    http: reqwest::blocking::Client,
    url: String,
    key: String,
    model: String,
}

pub struct Evaluation {
    pub model: String,
    pub input_tokens: Option<i64>,
    /// Question id to probability of yes.
    pub answers: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: BTreeMap<String, Answer>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Answer {
    noul: Option<f64>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<i64>,
}

impl Client {
    pub fn new(url: String, key: String, model: String) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Client {
            http,
            url,
            key,
            model,
        })
    }

    /// Asks every question as a noul (yes/no) over the same state in one request.
    pub fn noul(&self, state: &Value, questions: &BTreeMap<String, String>) -> Result<Evaluation> {
        let qs: serde_json::Map<String, Value> = questions
            .iter()
            .map(|(id, text)| (id.clone(), json!({"type": "noul", "instructions": text})))
            .collect();
        let body = json!({"state": state, "model": self.model, "questions": qs});
        let endpoint = format!("{}/v1/systemone", self.url.trim_end_matches('/'));

        let mut attempt = 0;
        let resp = loop {
            let resp = self
                .http
                .post(&endpoint)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .context("calling Jev")?;
            let status = resp.status().as_u16();
            if (status == 429 || status == 529) && attempt < RETRY_BACKOFF.len() {
                std::thread::sleep(RETRY_BACKOFF[attempt]);
                attempt += 1;
                continue;
            }
            if !resp.status().is_success() {
                let text = resp.text().unwrap_or_default();
                bail!(
                    "Jev returned HTTP {status} after {} attempt(s): {text}",
                    attempt + 1
                );
            }
            break resp;
        };
        let parsed: Response = resp.json().context("decoding Jev response")?;
        let mut answers = BTreeMap::new();
        for id in questions.keys() {
            let p = parsed
                .answers
                .get(id)
                .and_then(|a| a.noul)
                .with_context(|| format!("Jev response has no noul answer for question `{id}`"))?;
            answers.insert(id.clone(), p);
        }
        Ok(Evaluation {
            model: parsed.model,
            input_tokens: parsed.usage.and_then(|u| u.input_tokens),
            answers,
        })
    }
}
