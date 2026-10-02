//! Batched, cached, concurrent TypeSafe Jev (System One) Noul calls.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::scan::TestFn;

const TEST_QUESTION: &str = "Could the code change in `change` make `test` pass or fail differently than before, because `test` exercises the changed code directly or through functions, types, constants, data or behavior it depends on?";
const TEST_TRUE: &str =
    "The test calls, constructs or depends on changed code, or on behavior the change alters, so its outcome could change.";
const TEST_FALSE: &str = "The test only exercises code that the change neither touches nor affects.";
const NAMES_QUESTION: &str = "Judging from where `test` lives and what it is called, could the code change in `change` make `test` pass or fail differently than before, because `test` likely exercises the changed code directly or through functions, types, constants, data or behavior it depends on?";
const GROUP_QUESTION: &str = "Could the code change in `change` make any test in `group` pass or fail differently than before, because those tests exercise the changed code directly or through functions, types, constants, data or behavior they depend on?";
const GROUP_TRUE: &str = "The tests in the group call, construct or depend on changed code, or on behavior the change alters, so their outcome could change.";
const GROUP_FALSE: &str = "The tests in the group only exercise code that the change neither touches nor affects.";
const RETRY_STATUSES: [u16; 3] = [429, 503, 529];
const ATTEMPTS: u32 = 3;
/// Price per input token (output is free).
pub const USD_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;

pub struct Client {
    url: String,
    /// `None` = offline: answers come from the cache only.
    key: Option<String>,
    model: String,
    agent: ureq::Agent,
    cache_dir: PathBuf,
    /// Every request (retries included) ends by then: Jev never blocks past `jev.timeout_secs`.
    deadline: Instant,
    /// The first failure; later requests fail at once with it.
    dead: Mutex<Option<String>>,
}

/// Stage-2 `names` view: the test's identity only.
pub fn names_question(package: &str, test: &TestFn) -> Value {
    json!({
        "type": "noul",
        "instructions": {
            "test": {"package": package, "module": test.module, "name": test.name, "file": test.file},
            "question": NAMES_QUESTION,
        },
        "criteria": {"true": TEST_TRUE, "false": TEST_FALSE},
    })
}

/// Stage-2 `body` view: the test's source.
pub fn body_question(package: &str, test: &TestFn, max_test_chars: usize) -> Value {
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

/// Stage-1 screening question: every test of one (package, file, module) group.
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
    pub asked: usize,
    pub requests: u32,
    /// Questions answered from the cache.
    pub cache_hits: u32,
    /// Requests rejected as too large and retried as two halves.
    pub splits: u32,
    /// Offline: questions whose answer was not cached.
    pub uncached: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u128,
    pub failures: Vec<String>,
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.requests += other.requests;
        self.cache_hits += other.cache_hits;
        self.splits += other.splits;
        self.uncached += other.uncached;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.failures.extend(other.failures);
    }

    pub fn est_cost_usd(&self) -> f64 {
        self.input_tokens as f64 * USD_PER_INPUT_TOKEN
    }
}

enum Batch {
    Answered { nouls: Vec<f64>, input_tokens: u64, output_tokens: u64 },
    /// The API refused the request size (`max_tokens_exceeded`, 413 or 422).
    TooLarge { detail: String, cached: bool },
    /// Offline.
    Uncached,
    Failed(String),
}

/// One question's outcome.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Answer {
    Noul(f64),
    /// The request failed (after retries) or its answer was malformed.
    Failed,
    /// Offline and not in the cache.
    Uncached,
}

/// A request's answer and cost, for `doctor`.
pub struct Ping {
    pub noul: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub ms: u128,
}

impl Client {
    /// `key` = `None` makes the client offline (cache only). Every request this client makes,
    /// retries included, ends within `timeout_secs` of its creation.
    pub fn new(base_url: &str, model: &str, key: Option<String>, timeout_secs: u64, cache_dir: &Path) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .build();
        let _ = std::fs::create_dir_all(cache_dir);
        Client {
            url: format!("{}/systemone", base_url.trim_end_matches('/')),
            key,
            model: model.to_owned(),
            agent: ureq::Agent::new_with_config(config),
            cache_dir: cache_dir.to_owned(),
            deadline: Instant::now() + Duration::from_secs(timeout_secs),
            dead: Mutex::new(None),
        }
    }

    /// Asks one Noul per question. Each answer is cached on its own, keyed by the model, the
    /// state and the question, so a question asked again under the same change is free however
    /// the batches fall; only uncached questions are sent, up to `batch` per request. A request
    /// the API refuses as too large is retried as two halves, recursively. After the first failed
    /// request every later one fails at once with the same cause (any failure sends selection to
    /// `on_jev_error`).
    pub fn judge(&self, state: &Value, questions: &[Value], batch: usize, concurrency: usize) -> (Vec<Answer>, Usage) {
        let started = Instant::now();
        let mut usage = Usage { asked: questions.len(), ..Usage::default() };
        let state_digest = hex(&Sha256::digest(state.to_string().as_bytes()));
        let keys: Vec<PathBuf> = questions.iter().map(|q| self.question_path(&state_digest, q)).collect();
        let mut answers: Vec<Answer> = Vec::with_capacity(questions.len());
        let mut todo: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            match std::fs::read_to_string(key).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.get("noul")?.as_f64()) {
                Some(n) => {
                    usage.cache_hits += 1;
                    answers.push(Answer::Noul(n));
                }
                None => {
                    todo.push(i);
                    answers.push(Answer::Uncached);
                }
            }
        }

        let ask: Vec<Value> = todo.iter().map(|&i| questions[i].clone()).collect();
        let ask_keys: Vec<&Path> = todo.iter().map(|&i| keys[i].as_path()).collect();
        let chunks: Vec<(&[Value], &[&Path])> = ask.chunks(batch.max(1)).zip(ask_keys.chunks(batch.max(1))).collect();
        let next = AtomicUsize::new(0);
        let mut results: Vec<Option<(Vec<Answer>, Usage)>> = chunks.iter().map(|_| None).collect();
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..concurrency.clamp(1, chunks.len().max(1)))
                .map(|_| {
                    s.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(qs, ks)) = chunks.get(i) else { break };
                            let mut usage = Usage::default();
                            let nouls = self.ask_split(state, qs, ks, &mut usage);
                            done.push((i, (nouls, usage)));
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

        let mut fresh = Vec::with_capacity(todo.len());
        for (i, r) in results.into_iter().enumerate() {
            match r {
                Some((got, u)) => {
                    fresh.extend(got);
                    usage.add(u);
                }
                None => {
                    usage.failures.push(format!("batch {i}: worker panicked"));
                    fresh.extend(std::iter::repeat_n(Answer::Failed, chunks[i].0.len()));
                }
            }
        }
        for (&i, a) in todo.iter().zip(fresh) {
            answers[i] = a;
        }
        usage.wall_ms = started.elapsed().as_millis();
        (answers, usage)
    }

    fn body(&self, state: &Value, qs: &[Value]) -> String {
        let questions: Map<String, Value> = qs.iter().enumerate().map(|(i, q)| (format!("t{i}"), q.clone())).collect();
        json!({"model": self.model, "state": state, "questions": questions}).to_string()
    }

    /// Asks `qs` (cache files `keys`), splitting on size refusals; caches each answer.
    fn ask_split(&self, state: &Value, qs: &[Value], keys: &[&Path], usage: &mut Usage) -> Vec<Answer> {
        let body = self.body(state, qs);
        match self.ask(&body, qs.len(), true) {
            Batch::Answered { nouls, input_tokens, output_tokens } => {
                usage.requests += 1;
                usage.input_tokens += input_tokens;
                usage.output_tokens += output_tokens;
                for (key, n) in keys.iter().zip(&nouls) {
                    write_atomic(key, &json!({"noul": n}).to_string());
                }
                nouls.into_iter().map(Answer::Noul).collect()
            }
            Batch::TooLarge { cached, .. } if qs.len() > 1 => {
                if !cached {
                    usage.requests += 1;
                }
                usage.splits += 1;
                let mid = qs.len() / 2;
                let ((a, b), (ka, kb)) = (qs.split_at(mid), keys.split_at(mid));
                let (mut ua, mut ub) = (Usage::default(), Usage::default());
                let (mut nouls, rest) = std::thread::scope(|s| {
                    let first = s.spawn(|| self.ask_split(state, a, ka, &mut ua));
                    let rest = self.ask_split(state, b, kb, &mut ub);
                    (first.join().unwrap_or_else(|_| vec![Answer::Failed; a.len()]), rest)
                });
                usage.add(ua);
                usage.add(ub);
                nouls.extend(rest);
                nouls
            }
            Batch::Uncached => {
                usage.uncached += qs.len();
                vec![Answer::Uncached; qs.len()]
            }
            Batch::TooLarge { detail: why, .. } | Batch::Failed(why) => {
                usage.requests += 1;
                usage.failures.push(why);
                vec![Answer::Failed; qs.len()]
            }
        }
    }

    /// One question's cache file: sha256 of the model, the state's digest and the question.
    fn question_path(&self, state_digest: &str, question: &Value) -> PathBuf {
        let mut h = Sha256::new();
        h.update(b"jevtest question v1\0");
        h.update(self.model.as_bytes());
        h.update(b"\0");
        h.update(state_digest.as_bytes());
        h.update(b"\0");
        h.update(question.to_string().as_bytes());
        self.cache_dir.join(format!("q-{}.json", hex(&h.finalize())))
    }

    /// Sends one request. With `use_cache`, a size refusal cached for this exact body splits at
    /// once without a network call.
    fn ask(&self, body: &str, n: usize, use_cache: bool) -> Batch {
        let refusal = self.cache_dir.join(format!("{}.too-large", hex(&Sha256::digest(body.as_bytes()))));
        if use_cache && let Ok(detail) = std::fs::read_to_string(&refusal) {
            return Batch::TooLarge { detail, cached: true };
        }
        let Some(key) = &self.key else { return Batch::Uncached };
        if let Some(why) = self.dead.lock().ok().and_then(|d| d.clone()) {
            return Batch::Failed(why);
        }
        let batch = self.post(key, body, n, &refusal);
        if let Batch::Failed(why) = &batch
            && let Ok(mut dead) = self.dead.lock()
        {
            dead.get_or_insert_with(|| why.clone());
        }
        batch
    }

    fn post(&self, key: &str, body: &str, n: usize, refusal: &Path) -> Batch {
        let mut attempt = 0;
        let text = loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Batch::Failed("timed out (jev.timeout_secs)".into());
            }
            let sent = self
                .agent
                .post(&self.url)
                .config()
                .timeout_global(Some(left))
                .build()
                .header("Authorization", format!("Bearer {key}"))
                .content_type("application/json")
                .send(body.as_bytes());
            let mut resp = match sent {
                Ok(r) => r,
                Err(ureq::Error::Timeout(_)) => return Batch::Failed("timed out (jev.timeout_secs)".into()),
                Err(e) => return Batch::Failed(format!("cannot reach {}: {e}", self.url)),
            };
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            if status == 200 {
                break text;
            }
            attempt += 1;
            let what = match status {
                401 | 403 => " (API key rejected)",
                402 => " (out of credits)",
                500..=599 => " (server error)",
                _ => "",
            };
            let detail = format!("HTTP {status}{what}: {}", truncate(text.trim(), 200));
            if status == 413 || status == 422 || (status == 400 && text.contains("max_tokens_exceeded")) {
                write_atomic(refusal, &detail);
                return Batch::TooLarge { detail, cached: false };
            }
            let pause = Duration::from_millis(500 << (attempt - 1));
            let retry = RETRY_STATUSES.contains(&status) && attempt < ATTEMPTS && Instant::now() + pause < self.deadline;
            if !retry {
                return Batch::Failed(detail);
            }
            std::thread::sleep(pause);
        };
        parse(&text, n).unwrap_or_else(|why| Batch::Failed(format!("malformed answer: {why}")))
    }

    /// One tiny uncached question, for `doctor`.
    pub fn ping(&self) -> Result<Ping, String> {
        let state = json!({"change": {"diff": "-fn add(a: i32, b: i32) -> i32 { a + b }\n+fn add(a: i32, b: i32) -> i32 { a - b }"}});
        let question = json!({
            "type": "noul",
            "instructions": {"test": {"name": "adds_two_numbers", "source": "assert_eq!(add(2, 2), 4);"}, "question": TEST_QUESTION},
            "criteria": {"true": TEST_TRUE, "false": TEST_FALSE},
        });
        let started = Instant::now();
        match self.ask(&self.body(&state, &[question]), 1, false) {
            Batch::Answered { nouls, input_tokens, output_tokens, .. } => {
                Ok(Ping { noul: nouls[0], input_tokens, output_tokens, ms: started.elapsed().as_millis() })
            }
            Batch::TooLarge { detail, .. } | Batch::Failed(detail) => Err(detail),
            Batch::Uncached => Err("no API key".into()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
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
