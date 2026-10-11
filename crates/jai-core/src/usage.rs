//! Token, context and elapsed-time accounting for the status line.
//!
//! One line, three facts:
//!
//! ```text
//! Tokens: ~12,400 | Cxt: 345,000 / 1,000,000 (34.5%) | Time Used: 2m 12.4s
//! ```
//!
//! * `Tokens` is the *task* total (prompt + completion) for the running session,
//!   taken from provider usage whenever the provider reports it and estimated
//!   otherwise — estimated numbers are prefixed with `~`.
//! * `Cxt` is the *active context*: what the next request would actually send
//!   (system prompt, tool schemas, conversation), not the sum of saved logs.
//! * `Time Used` is wall-clock time for the current task (one user turn).

use crate::message::TokenUsage;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Tokens a task has consumed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// True when at least part of the total was estimated locally.
    pub estimated: bool,
}

impl TaskUsage {
    /// Provider-reported totals.
    pub fn reported(usage: &TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            estimated: false,
        }
    }

    /// Locally estimated totals (the provider reported nothing).
    pub fn estimated(prompt_tokens: u64, completion_tokens: u64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            estimated: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0
    }

    /// Merge provider numbers into the running total, keeping the estimate only
    /// for the parts the provider did not report.
    pub fn add_reported(&mut self, usage: &TokenUsage) {
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.total_tokens += usage.total_tokens;
    }
}

/// Size of the active context against the model's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Tokens the next request would carry.
    pub tokens: u64,
    /// Model context window (0 when unknown).
    pub window: u64,
    /// True when `tokens` came from the local estimator.
    pub estimated: bool,
}

impl ContextUsage {
    pub fn new(tokens: u64, window: u64, estimated: bool) -> Self {
        Self {
            tokens,
            window,
            estimated,
        }
    }

    /// Locally estimated context size.
    pub fn estimate(tokens: u64, window: u64) -> Self {
        Self::new(tokens, window, true)
    }

    /// Context size derived from provider-reported usage for the request that
    /// was just sent (prompt + the completion that stays in the transcript).
    pub fn reported(usage: &TokenUsage, window: u64) -> Self {
        Self::new(usage.prompt_tokens + usage.completion_tokens, window, false)
    }

    /// Percentage of the window in use, when the window is known.
    pub fn percent(&self) -> Option<f64> {
        if self.window == 0 {
            return None;
        }
        Some(self.tokens as f64 * 100.0 / self.window as f64)
    }

    /// True once the context is close enough to the window to be worth warning
    /// about (the UI colours the line, and the engine mentions it once).
    pub fn is_near_limit(&self) -> bool {
        self.percent().map(|percent| percent >= 90.0).unwrap_or(false)
    }
}

/// The three facts of the status line, each optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageLine {
    pub task: Option<TaskUsage>,
    pub context: Option<ContextUsage>,
    pub elapsed: Option<Duration>,
}

impl UsageLine {
    pub fn render(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        if let Some(task) = &self.task {
            segments.push(format!("Tokens: {}", counts(task.total_tokens, task.estimated)));
        }
        if let Some(context) = &self.context {
            segments.push(format!("Cxt: {}", context.render()));
        }
        if let Some(elapsed) = self.elapsed {
            segments.push(format!("Time Used: {}", format_duration(elapsed)));
        }
        segments.join(" | ")
    }
}

impl ContextUsage {
    /// `345,000 / 1,000,000 (34.5%)`, or just the token count when the window is
    /// unknown.
    pub fn render(&self) -> String {
        let tokens = counts(self.tokens, self.estimated);
        match self.percent() {
            Some(percent) => format!(
                "{tokens} / {} ({percent:.1}%)",
                format_count(self.window)
            ),
            None => tokens,
        }
    }
}

fn counts(value: u64, estimated: bool) -> String {
    if estimated {
        format!("~{}", format_count(value))
    } else {
        format_count(value)
    }
}

/// `12400` → `12,400`.
pub fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `45.3s`, `2m 12.4s`, `1h 02m 03s`.
pub fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let hours = (seconds / 3600.0).floor() as u64;
    let minutes = ((seconds - hours as f64 * 3600.0) / 60.0).floor() as u64;
    let rest = seconds - hours as f64 * 3600.0 - minutes as f64 * 60.0;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {rest:04.1}s")
    } else {
        format!("{minutes}m {rest:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens;

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(12_400), "12,400");
        assert_eq!(format_count(345_000), "345,000");
        assert_eq!(format_count(1_000_000), "1,000,000");
    }

    #[test]
    fn durations_read_like_the_status_line() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0.0s");
        assert_eq!(format_duration(Duration::from_millis(45_300)), "45.3s");
        assert_eq!(format_duration(Duration::from_millis(132_400)), "2m 12.4s");
        assert_eq!(
            format_duration(Duration::from_secs(3_723)),
            "1h 02m 03.0s"
        );
    }

    #[test]
    fn the_status_line_matches_the_documented_shape() {
        let line = UsageLine {
            task: Some(TaskUsage {
                prompt_tokens: 12_000,
                completion_tokens: 400,
                total_tokens: 12_400,
                estimated: true,
            }),
            context: Some(ContextUsage::new(345_000, 1_000_000, false)),
            elapsed: Some(Duration::from_millis(132_400)),
        };
        assert_eq!(
            line.render(),
            "Tokens: ~12,400 | Cxt: 345,000 / 1,000,000 (34.5%) | Time Used: 2m 12.4s"
        );
    }

    #[test]
    fn missing_segments_are_omitted_not_faked() {
        assert_eq!(UsageLine::default().render(), "");
        assert_eq!(
            UsageLine {
                task: Some(TaskUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    estimated: false,
                }),
                ..UsageLine::default()
            }
            .render(),
            "Tokens: 0"
        );
        // An unknown window drops the ratio instead of printing "/ 0 (0%)".
        assert_eq!(
            UsageLine {
                context: Some(ContextUsage::new(1_500, 0, true)),
                ..UsageLine::default()
            }
            .render(),
            "Cxt: ~1,500"
        );
    }

    #[test]
    fn context_percentages_and_limits() {
        let context = ContextUsage::new(64_000, 128_000, true);
        assert_eq!(context.percent(), Some(50.0));
        assert!(!context.is_near_limit());
        assert!(ContextUsage::new(120_000, 128_000, false).is_near_limit());
        assert_eq!(ContextUsage::new(1, 0, false).percent(), None);
        assert!(!ContextUsage::new(1, 0, false).is_near_limit());
    }

    #[test]
    fn provider_usage_beats_the_estimator() {
        let reported = TaskUsage::reported(&TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
        });
        assert_eq!(reported.total_tokens, 120);
        assert!(!reported.estimated);
        assert_eq!(TaskUsage::estimated(100, 20).total_tokens, 120);
        assert!(TaskUsage::estimated(100, 20).estimated);
        assert!(TaskUsage::default().is_empty());

        let mut running = reported;
        running.add_reported(&TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        });
        assert_eq!(running.total_tokens, 123);
    }

    #[test]
    fn the_estimator_is_used_when_nothing_is_reported() {
        let context = ContextUsage::estimate(
            tokens::estimate_tokens("hello world"),
            tokens::resolve_context_window("deepseek-chat", None),
        );
        assert!(context.estimated);
        assert_eq!(context.window, 128_000);
        assert!(context.render().starts_with("Cxt: ~"), "{}", context.render());
    }
}
