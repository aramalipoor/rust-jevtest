//! Batched, cached, concurrent TypeSafe Jev (System One) Noul calls.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::scan::TestFn;

const TEST_QUESTION: &str = "Could the code change in `change` make `test` pass or fail differently than before, because `test` exercises the changed code directly or through functions, types, constants, data or behavior it depends on?";
const TEST_TRUE: &str =
    "The test calls, constructs or depends on changed code, or on behavior the change alters, so its outcome could change.";
const TEST_FALSE: &str = "The test only exercises code that the change neither touches nor affects.";
const GROUP_QUESTION: &str = "Could the code change in `change` make any test in `group` pass or fail differently than before, because those tests exercise the changed code directly or through functions, types, constants, data or behavior they depend on?";
const GROUP_TRUE: &str = "The tests in the group call, construct or depend on changed code, or on behavior the change alters, so their outcome could change.";
const GROUP_FALSE: &str = "The tests in the group only exercise code that the change neither touches nor affects.";
const RETRY_STATUSES: [u16; 3] = [429, 503, 529];
const ATTEMPTS: u32 = 3;

pub struct Client {
    url: String,
    key: String,
    model: String,
    agent: ureq::Agent,
    cache_dir: Option<PathBuf>,
}

/// Stage-2 question: one test.
pub fn test_question(package: &str, test: &TestFn, max_test_chars: usize) -> Value {
    json!({
        "type": "noul",
        "instructions": {
            "test": {
                "package": package,
                "module": test.module,
                "name": test.name,
                "file": test.file,
                "source": truncate(&test.source, max_test_chars),
            },
            "question": TEST_QUESTION,
        },
        "criteria": {"true": TEST_TRUE, "false": TEST_FALSE},
    })
}

/// Stage-1 question: every test of one (package, file, module) group.
pub fn group_question(package: &str, file: &str, module: &str, tests: &[&str], context: &str) -> Value {
    json!({
        "type": "noul",
        "instructions": {
            "group": {
                "package": package,
                "file": file,
                "module": module,
                "tests": tests,
                "context": context,
            },
            "question": GROUP_QUESTION,
        },
        "criteria": {"true": GROUP_TRUE, "false": GROUP_FALSE},
    })
}

#[derive(Default)]
pub struct Usage {
    pub requests: u32,
    pub cache_hits: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u128,
    pub failures: Vec<String>,
}

enum Batch {
    Answered { nouls: Vec<f64>, cached: bool, input_tokens: u64, output_tokens: u64 },
    Failed(String),
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

impl Client {
    pub fn from_env() -> Result<Self, String> {
        let key = match std::env::var("TYPESAFE_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k.trim().to_owned(),
            _ => {
                let path = home().ok_or("HOME is unset")?.join(".config/jevtest/typesafe.key");
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("no TYPESAFE_API_KEY and cannot read {}: {e}", path.display()))?
                    .trim()
                    .to_owned()
            }
        };
        if key.is_empty() {
            return Err("TypeSafe API key is empty".into());
        }
        let base = std::env::var("TYPESAFE_BASE_URL").unwrap_or_else(|_| "https://api.typesafe.ai/v1".into());
        let model = std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| "jev-latest".into());
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .build();
        let cache_dir = home().map(|h| h.join(".cache/jevtest"));
        if let Some(dir) = &cache_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        Ok(Client {
            url: format!("{}/systemone", base.trim_end_matches('/')),
            key,
            model,
            agent: ureq::Agent::new_with_config(config),
            cache_dir,
        })
    }

    /// Asks one Noul per question, `batch` per request; `None` for questions whose batch failed (unjudged).
    pub fn judge(&self, state: &Value, questions: Vec<Value>, batch: usize, concurrency: usize) -> (Vec<Option<f64>>, Usage) {
        let started = Instant::now();
        let total = questions.len();
        let mut bodies: Vec<String> = Vec::new();
        let mut it = questions.into_iter().peekable();
        while it.peek().is_some() {
            let qs: Map<String, Value> = it.by_ref().take(batch.max(1)).enumerate().map(|(i, q)| (format!("t{i}"), q)).collect();
            bodies.push(json!({"model": self.model, "state": state, "questions": qs}).to_string());
        }
        let next = AtomicUsize::new(0);
        let mut results: Vec<Option<Batch>> = bodies.iter().map(|_| None).collect();
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..concurrency.clamp(1, bodies.len().max(1)))
                .map(|_| {
                    s.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(body) = bodies.get(i) else { break };
                            let n = total.min((i + 1) * batch.max(1)) - i * batch.max(1);
                            done.push((i, self.ask(body, n)));
                        }
                        done
                    })
                })
                .collect();
            for w in workers {
                for (i, r) in w.join().unwrap_or_default() {
                    results[i] = Some(r);
                }
            }
        });

        let mut usage = Usage::default();
        let mut nouls = Vec::with_capacity(total);
        for (i, r) in results.into_iter().enumerate() {
            let n = total.min((i + 1) * batch.max(1)) - i * batch.max(1);
            match r.unwrap_or_else(|| Batch::Failed("worker panicked".into())) {
                Batch::Answered { nouls: got, cached, input_tokens, output_tokens } => {
                    if cached {
                        usage.cache_hits += 1;
                    } else {
                        usage.requests += 1;
                        usage.input_tokens += input_tokens;
                        usage.output_tokens += output_tokens;
                    }
                    nouls.extend(got.into_iter().map(Some));
                }
                Batch::Failed(why) => {
                    usage.requests += 1;
                    usage.failures.push(format!("batch {i}: {why}"));
                    nouls.extend(std::iter::repeat_n(None, n));
                }
            }
        }
        usage.wall_ms = started.elapsed().as_millis();
        (nouls, usage)
    }

    fn cache_path(&self, body: &str) -> Option<PathBuf> {
        let digest = Sha256::digest(body.as_bytes());
        let mut hex = String::with_capacity(64);
        for b in digest.iter() {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        self.cache_dir.as_ref().map(|d| d.join(format!("{hex}.json")))
    }

    fn ask(&self, body: &str, n: usize) -> Batch {
        let cache = self.cache_path(body);
        if let Some(path) = &cache
            && let Ok(text) = std::fs::read_to_string(path)
            && let Ok(Batch::Answered { nouls, input_tokens, output_tokens, .. }) = parse(&text, n)
        {
            return Batch::Answered { nouls, cached: true, input_tokens, output_tokens };
        }
        let mut attempt = 0;
        let text = loop {
            let sent = self
                .agent
                .post(&self.url)
                .header("Authorization", format!("Bearer {}", self.key))
                .content_type("application/json")
                .send(body.as_bytes());
            let mut resp = match sent {
                Ok(r) => r,
                Err(e) => return Batch::Failed(format!("request failed: {e}")),
            };
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            if status == 200 {
                break text;
            }
            attempt += 1;
            if !RETRY_STATUSES.contains(&status) || attempt >= ATTEMPTS {
                return Batch::Failed(format!("HTTP {status}: {}", truncate(text.trim(), 300)));
            }
            std::thread::sleep(Duration::from_millis(500 << (attempt - 1)));
        };
        match parse(&text, n) {
            Ok(answered) => {
                if let Some(path) = &cache {
                    let tmp = path.with_extension("tmp");
                    if std::fs::write(&tmp, &text).is_ok() {
                        let _ = std::fs::rename(&tmp, path);
                    }
                }
                answered
            }
            Err(why) => Batch::Failed(why),
        }
    }
}

fn parse(text: &str, n: usize) -> Result<Batch, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad response JSON: {e}"))?;
    let answers = v.get("answers").ok_or("response has no answers")?;
    let nouls = (0..n)
        .map(|i| answers.get(format!("t{i}")).and_then(|a| a.get("noul")).and_then(Value::as_f64))
        .collect::<Option<Vec<f64>>>()
        .ok_or("batch missing answers")?;
    let tokens = |k: &str| v.get("usage").and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    Ok(Batch::Answered {
        nouls,
        cached: false,
        input_tokens: tokens("input_tokens"),
        output_tokens: tokens("output_tokens"),
    })
}

/// At most `max` chars of `s`, cut on a char boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
