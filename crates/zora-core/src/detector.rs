//! Extracts commands delimited by start/end phrases from final transcript segments (port of v1 CommandDetector).

use crate::clock::{Ns, secs};
use crate::model::{Command, DiscardReason, Segment};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// One normalized token plus the original text around it, so prompts are sliced exactly at token boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextUnit {
    pub norm: String,
    /// Whitespace and opening punctuation before the token.
    pub leading: String,
    /// The token as written plus trailing punctuation attached to it.
    pub original: String,
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// Lowercase, strip diacritics and width/compatibility forms, keep letters and digits only.
pub fn fold(s: &str) -> String {
    s.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub fn units(text: &str) -> Vec<TextUnit> {
    let mut out = Vec::new();
    let (mut leading, mut token, mut trailing) = (String::new(), String::new(), String::new());

    fn finish(
        out: &mut Vec<TextUnit>,
        leading: &mut String,
        token: &mut String,
        trailing: &mut String,
    ) {
        if token.is_empty() {
            return;
        }
        let norm = fold(token);
        if norm.is_empty() {
            leading.push_str(token);
            leading.push_str(trailing);
        } else {
            out.push(TextUnit {
                norm,
                leading: std::mem::take(leading),
                original: format!("{token}{trailing}"),
            });
        }
        token.clear();
        trailing.clear();
    }

    for ch in text.chars() {
        if ch.is_alphanumeric() || (is_apostrophe(ch) && !token.is_empty() && trailing.is_empty()) {
            if !trailing.is_empty() {
                finish(&mut out, &mut leading, &mut token, &mut trailing);
            }
            token.push(ch);
        } else if ch.is_whitespace() {
            finish(&mut out, &mut leading, &mut token, &mut trailing);
            trailing.clear();
            leading.push(ch);
        } else if is_combining_mark(ch) && !token.is_empty() {
            // Decomposed accents belong to the letter before them.
            if trailing.is_empty() {
                token.push(ch)
            } else {
                trailing.push(ch)
            }
        } else if token.is_empty() {
            leading.push(ch);
        } else {
            trailing.push(ch);
        }
    }
    finish(&mut out, &mut leading, &mut token, &mut trailing);
    out
}

pub fn tokens(text: &str) -> Vec<String> {
    units(text).into_iter().map(|u| u.norm).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetectorConfig {
    pub start: Vec<Vec<String>>,
    pub end: Vec<Vec<String>>,
    pub cancel: Vec<Vec<String>>,
    pub capture_timeout: Ns,
    pub max_gap: Ns,
    pub max_chars: usize,
    pub idle_tail_tokens: usize,
    pub idle_tail_ttl: Ns,
    /// The assistant's names (aliases). In a phrase, the word "zora" or any of these names is a name slot:
    /// it matches every configured name and their sound-alikes ("zara", "zura", "sora" for "zora"), because
    /// speech engines rarely spell a made-up name right and not everyone pronounces the same name.
    pub names: Vec<String>,
}

/// Whether a transcribed token is a sound-alike of `name`: same word after folding a leading s→z, doubled
/// letters and a trailing h, or one edit away from it with the same first letter.
pub fn name_like(token: &str, name: &str) -> bool {
    fn shape(w: &str) -> Vec<char> {
        let mut out: Vec<char> = Vec::new();
        for (i, c) in w.chars().enumerate() {
            let c = if i == 0 && c == 's' { 'z' } else { c };
            if out.last() != Some(&c) {
                out.push(c);
            }
        }
        if out.len() > 3 && out.last() == Some(&'h') {
            out.pop();
        }
        out
    }
    let (a, b) = (shape(token), shape(name));
    if a == b {
        return true;
    }
    if a.len() < 3 || a.first() != b.first() || a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    // Levenshtein distance <= 1.
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < short.len() && j < long.len() {
        if short[i] == long[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if short.len() == long.len() {
            i += 1;
        }
        j += 1;
    }
    edits + (long.len() - j) + (short.len() - i) <= 1
}

impl DetectorConfig {
    pub fn with_names(mut self, names: &[String]) -> Self {
        self.names = names
            .iter()
            .map(|n| fold(n))
            .filter(|n| !n.is_empty())
            .collect();
        self
    }

    /// Whether `text` contains any start phrase (used on live, unfinished transcripts for early feedback only).
    pub fn mentions_start(&self, text: &str) -> bool {
        let toks = tokens(text);
        self.start.iter().any(|phrase| {
            toks.iter().any(|t| self.fused_match(t, phrase))
                || phrase.len() <= toks.len()
                    && (0..=toks.len() - phrase.len()).any(|i| {
                        phrase
                            .iter()
                            .enumerate()
                            .all(|(k, p)| self.token_matches(&toks[i + k], p))
                    })
        })
    }

    /// "hazora", "heyzora", "anova": a greeting fused onto a name by the speech engine. Matches a two-word
    /// phrase `[greeting, name-slot]` with a single transcribed token.
    fn fused_match(&self, token: &str, phrase: &[String]) -> bool {
        let [greeting, name] = phrase else {
            return false;
        };
        if !self.is_name_slot(name) {
            return false;
        }
        let own = [greeting.as_str()];
        let prefixes: &[&str] = match greeting.as_str() {
            "hey" | "hay" | "hi" => &["hey", "hay", "hei", "he", "ha", "hi", "a"],
            "okay" | "ok" => &["okay", "okey", "ok", "oke"],
            _ => &own,
        };
        prefixes.iter().any(|p| {
            token
                .strip_prefix(p)
                .is_some_and(|rest| rest.len() >= 3 && self.matches_name(rest))
        })
    }

    fn token_matches(&self, t: &str, p: &str) -> bool {
        t == p || (self.is_name_slot(p) && self.matches_name(t))
    }

    fn is_name_slot(&self, phrase_token: &str) -> bool {
        phrase_token == "zora" || self.names.iter().any(|n| n == phrase_token)
    }

    fn matches_name(&self, token: &str) -> bool {
        token == "zora" && self.names.is_empty() || self.names.iter().any(|n| name_like(token, n))
    }

    pub fn new(
        start: &[String],
        end: &[String],
        cancel: &[String],
        capture_timeout: f64,
        max_gap: f64,
        max_chars: usize,
    ) -> Self {
        let phrases = |list: &[String]| {
            list.iter()
                .map(|p| tokens(p))
                .filter(|t| !t.is_empty())
                .collect()
        };
        DetectorConfig {
            start: phrases(start),
            end: phrases(end),
            cancel: phrases(cancel),
            capture_timeout: secs(capture_timeout),
            max_gap: secs(max_gap),
            max_chars,
            idle_tail_tokens: 6,
            idle_tail_ttl: secs(3.0),
            names: vec!["zora".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectorOutput {
    CaptureStarted { at: Ns },
    Command(Command),
    Discarded(DiscardReason),
}

#[derive(Debug, Clone)]
struct Unit {
    text: TextUnit,
    time: Ns,
}

#[derive(Debug, Clone)]
pub struct CommandDetector {
    pub config: DetectorConfig,
    units: Vec<Unit>,
    capturing: bool,
    capture_start: Ns,
    last_activity: Ns,
    next_id: u64,
}

impl CommandDetector {
    pub fn new(config: DetectorConfig) -> Self {
        CommandDetector {
            config,
            units: Vec::new(),
            capturing: false,
            capture_start: 0,
            last_activity: Ns::MIN / 2,
            next_id: 0,
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    pub fn capture_started_at(&self) -> Option<Ns> {
        self.capturing.then_some(self.capture_start)
    }

    pub fn feed(&mut self, seg: &Segment) -> Vec<DetectorOutput> {
        let mut out = Vec::new();
        if self.capturing && seg.start - self.last_activity > self.config.max_gap {
            out.extend(self.abort(DiscardReason::Gap));
        }
        if !self.capturing
            && let Some(last) = self.units.last()
            && seg.start - last.time > self.config.idle_tail_ttl
        {
            self.units.clear();
        }
        let mut parts = units(&seg.text);
        if !self.units.is_empty()
            && let Some(first) = parts.first_mut()
            && first.leading.is_empty()
        {
            first.leading = " ".into();
        }
        // Spread token times across the segment so a phrase start gets a sensible timestamp.
        let step = if parts.is_empty() {
            0
        } else {
            (seg.end - seg.start) / parts.len() as Ns
        };
        for (i, p) in parts.into_iter().enumerate() {
            self.units.push(Unit {
                text: p,
                time: seg.start + step * i as Ns,
            });
        }
        self.last_activity = self.last_activity.max(seg.end);
        out.extend(self.scan());
        out
    }

    /// Ends an open capture. When idle it only clears the tail.
    pub fn abort(&mut self, reason: DiscardReason) -> Vec<DetectorOutput> {
        let was = self.capturing;
        self.capturing = false;
        self.units.clear();
        if was {
            vec![DetectorOutput::Discarded(reason)]
        } else {
            vec![]
        }
    }

    pub fn tick(&mut self, now: Ns) -> Vec<DetectorOutput> {
        if !self.capturing {
            if let Some(last) = self.units.last()
                && now - last.time > self.config.idle_tail_ttl + secs(1.0)
            {
                self.units.clear();
            }
            return vec![];
        }
        if now - self.capture_start > self.config.capture_timeout {
            return self.abort(DiscardReason::Timeout);
        }
        if now - self.last_activity > self.config.max_gap {
            return self.abort(DiscardReason::Gap);
        }
        vec![]
    }

    fn scan(&mut self) -> Vec<DetectorOutput> {
        let mut out = Vec::new();
        loop {
            if !self.capturing {
                let Some((first, last)) = self.first_match(&self.config.start) else {
                    let keep = self.config.idle_tail_tokens;
                    if self.units.len() > keep {
                        self.units.drain(..self.units.len() - keep);
                    }
                    return out;
                };
                self.capturing = true;
                self.capture_start = self.units[first].time;
                self.units.drain(..=last);
                out.push(DetectorOutput::CaptureStarted {
                    at: self.capture_start,
                });
                continue;
            }

            let end = self.first_match(&self.config.end);
            let cancel = self.first_match(&self.config.cancel);
            // A new wake phrase before any end/cancel phrase restarts the capture: the latest "Hey Zora" wins,
            // so a missed end phrase does not swallow everything said afterwards. A wake phrase that overlaps
            // an end phrase ("okay zora, do it") is part of that end phrase, not a restart.
            if let Some((r_first, r_last)) = self.first_match(&self.config.start)
                && end.is_none_or(|(e_first, _)| r_last < e_first)
                && cancel.is_none_or(|(c_first, _)| r_last < c_first)
            {
                self.capture_start = self.units[r_first].time;
                self.units.drain(..=r_last);
                out.push(DetectorOutput::CaptureStarted {
                    at: self.capture_start,
                });
                continue;
            }
            if let Some((c_first, c_last)) = cancel
                && end.is_none_or(|(e_first, _)| c_first < e_first)
            {
                self.capturing = false;
                self.units.drain(..=c_last);
                out.push(DetectorOutput::Discarded(DiscardReason::Cancelled));
                continue;
            }
            if let Some((e_first, e_last)) = end {
                let raw: String = self.units[..e_first]
                    .iter()
                    .map(|u| format!("{}{}", u.text.leading, u.text.original))
                    .collect();
                let text = clean(&raw);
                let end_time = self.units[e_last].time;
                self.capturing = false;
                self.units.drain(..=e_last);
                // Limits apply even when both delimiters arrive in one segment.
                if text.is_empty() {
                    out.push(DetectorOutput::Discarded(DiscardReason::Empty));
                } else if text.chars().count() > self.config.max_chars {
                    out.push(DetectorOutput::Discarded(DiscardReason::TooLong));
                } else if end_time - self.capture_start > self.config.capture_timeout {
                    out.push(DetectorOutput::Discarded(DiscardReason::Timeout));
                } else {
                    self.next_id += 1;
                    out.push(DetectorOutput::Command(Command {
                        id: uuid::Uuid::new_v4().to_string(),
                        text,
                        capture_start: self.capture_start,
                        capture_end: end_time,
                    }));
                }
                continue;
            }
            let length: usize = self
                .units
                .iter()
                .map(|u| u.text.leading.chars().count() + u.text.original.chars().count())
                .sum();
            if length > self.config.max_chars {
                out.extend(self.abort(DiscardReason::TooLong));
            }
            return out;
        }
    }

    fn first_match(&self, phrases: &[Vec<String>]) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for phrase in phrases {
            if let Some(i) = self
                .units
                .iter()
                .position(|u| self.config.fused_match(&u.text.norm, phrase))
                && best.is_none_or(|(b, _)| i < b)
            {
                best = Some((i, i));
            }
            if phrase.len() > self.units.len() {
                continue;
            }
            for i in 0..=(self.units.len() - phrase.len()) {
                if best.is_some_and(|(b, _)| i >= b) {
                    break;
                }
                let cfg = &self.config;
                if phrase
                    .iter()
                    .enumerate()
                    .all(|(k, p)| cfg.token_matches(&self.units[i + k].text.norm, p))
                {
                    best = Some((i, i + phrase.len() - 1));
                    break;
                }
            }
        }
        best
    }
}

/// Unicode punctuation (general category P*) for the characters transcripts contain; symbols such as `$` or
/// a backtick are not punctuation, matching Foundation's `punctuationCharacters` used by the Swift core.
fn is_punctuation(c: char) -> bool {
    if c.is_ascii() {
        return matches!(
            c,
            '!' | '"'
                | '#'
                | '%'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | ','
                | '-'
                | '.'
                | '/'
                | ':'
                | ';'
                | '?'
                | '@'
                | '['
                | '\\'
                | ']'
                | '_'
                | '{'
                | '}'
        );
    }
    matches!(
        c,
        '…' | '“'
            | '”'
            | '‘'
            | '’'
            | '«'
            | '»'
            | '¿'
            | '¡'
            | '—'
            | '–'
            | '‐'
            | '‑'
            | '·'
            | '、'
            | '。'
            | '，'
            | '！'
            | '？'
            | '：'
            | '；'
            | '„'
            | '‚'
            | '‹'
            | '›'
    )
}

/// Trims whitespace and punctuation from both ends.
pub fn clean(s: &str) -> String {
    s.trim_matches(|c: char| c.is_whitespace() || is_punctuation(c))
        .to_string()
}
