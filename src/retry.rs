//! Shared retry primitive for model calls that intermittently return no
//! usable content.
//!
//! [`RetryReason`] classifies why a single attempt failed, and the module owns
//! which reasons are retryable — so it never names a rig type. The single
//! rig→reason mapping lives at the [`crate::llm`] boundary
//! ([`crate::llm::classify_retry`]); this module is provider-agnostic.
//!
//! Four retry seams share it: the Drafted-Message (`schema`) and
//! untyped-completion (`call`) paths via [`retry`] + [`RetryPolicy::transient`],
//! the batch-plan streaming inline loop via [`should_retry`] +
//! [`RetryPolicy::transient`], and the resolve conflict-marker workflow via
//! [`retry`] + [`RetryPolicy::once`].

use std::time::Duration;

/// Why a single model-call attempt failed.
///
/// `Empty` and `Truncated` are the two "model returned no usable content"
/// shapes a budget-starved reasoning model produces; both are retryable.
/// `Markers` is the resolve workflow's re-roll: the resolver returned
/// marker-laden output, which gets one retry via [`RetryPolicy::once`].
/// [`RetryReason::Fatal`] is anything else — it propagates immediately, never
/// retried, carrying the original error verbatim.
#[derive(Debug)]
pub enum RetryReason {
    /// The model returned no content at all (empty completion).
    Empty,
    /// The model returned content truncated mid-generation so it won't parse.
    Truncated,
    /// The output contained conflict markers requiring a re-roll.
    Markers,
    /// A non-content failure (auth, rate limit, network, …) carried verbatim
    /// so it propagates unchanged to the caller.
    Fatal(anyhow::Error),
}

impl RetryReason {
    /// Whether this reason is worth retrying — the module's own knowledge, so
    /// callers never have to re-derive it. `Fatal` is never retryable.
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Empty | Self::Truncated | Self::Markers)
    }
}

/// Policy controlling a retry loop's budget and backoff.
///
/// Opaque by design: callers pick a named constructor
/// ([`Self::transient`] / [`Self::once`]) and the module decides the
/// mechanics, so the budget and backoff can't drift between retry seams.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Extra attempts beyond the first.
    budget: usize,
    /// Base step (ms) of the linear backoff; `None` means no backoff.
    backoff_base_ms: Option<u64>,
}

impl RetryPolicy {
    /// Budget 2 with a short linear backoff — the historical retry behavior for
    /// budget-starved reasoning models (DeepSeek et al. blowing their output
    /// budget on `reasoning_content`). Backoff steps: 300 ms, then 600 ms.
    pub const fn transient() -> Self {
        Self {
            budget: 2,
            backoff_base_ms: Some(300),
        }
    }

    /// Budget 1 with no backoff — the resolve workflow's conflict-marker
    /// re-roll: marker-laden output gets exactly one retry, immediately.
    pub const fn once() -> Self {
        Self {
            budget: 1,
            backoff_base_ms: None,
        }
    }

    /// Linear backoff for the `attempt`-th retry (1-indexed). `None` policy
    /// backoff (e.g. [`Self::once`]) yields zero — retry immediately.
    fn backoff(&self, attempt: usize) -> Duration {
        match self.backoff_base_ms {
            Some(base) => Duration::from_millis(base * attempt as u64),
            None => Duration::ZERO,
        }
    }
}

/// Exhaustion record returned when every attempt produced a retryable
/// [`RetryReason`] and the policy's budget is spent.
#[derive(Debug)]
pub struct RetryExhausted {
    /// Extra attempts beyond the first that were performed (equals the policy
    /// budget on exhaustion).
    pub attempts: usize,
    /// The last retryable reason the operation produced.
    pub last_reason: RetryReason,
}

/// The error of a retry loop. Either the budget was exhausted by retryable
/// reasons ([`RetryError::Exhausted`]), or a [`RetryReason::Fatal`] propagated
/// immediately carrying its original error ([`RetryError::Fatal`]).
#[derive(Debug)]
pub enum RetryError {
    Exhausted(RetryExhausted),
    Fatal(anyhow::Error),
}

impl std::fmt::Display for RetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted(RetryExhausted {
                attempts,
                last_reason,
            }) => write!(
                f,
                "model call returned no usable content after {attempts} \
                 {attempt_word}; last failure: {last_reason:?}",
                attempt_word = if *attempts == 1 { "retry" } else { "retries" },
            ),
            Self::Fatal(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for RetryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exhausted(_) => None,
            Self::Fatal(err) => Some(err.as_ref()),
        }
    }
}

/// The retry decision spine: if `reason` is retryable and the budget tracked by
/// `attempts` isn't spent, bump `attempts` and return the backoff to wait
/// before the next attempt; otherwise return `None`, meaning the caller should
/// stop retrying and surface the failure.
///
/// This is the single budget gate both [`retry`] and the streaming inline loop
/// in [`crate::llm::LLMAgent::stream_typed_with_reasoning`] call, so the seams
/// can't drift.
pub fn should_retry(
    reason: &RetryReason,
    attempts: &mut usize,
    policy: RetryPolicy,
) -> Option<Duration> {
    if reason.is_retryable() && *attempts < policy.budget {
        *attempts += 1;
        Some(policy.backoff(*attempts))
    } else {
        None
    }
}

/// Run `op`, retrying retryable [`RetryReason`]s up to `policy`'s budget with
/// its backoff. A [`RetryReason::Fatal`] propagates immediately (as
/// [`RetryError::Fatal`]); retryable exhaustion surfaces as
/// [`RetryError::Exhausted`].
pub async fn retry<T, F, Fut>(mut op: F, policy: RetryPolicy) -> Result<T, RetryError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RetryReason>>,
{
    let mut attempts = 0usize;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(reason) => match should_retry(&reason, &mut attempts, policy) {
                Some(backoff) => {
                    // `once()` has no backoff — retry immediately, no timer.
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                }
                None => return Err(into_retry_error(reason, attempts)),
            },
        }
    }
}

/// Map a terminal reason to its [`RetryError`]: a retryable reason (budget
/// spent) becomes [`RetryError::Exhausted`]; a [`RetryReason::Fatal`] unwraps
/// to [`RetryError::Fatal`], preserving the original error.
fn into_retry_error(reason: RetryReason, attempts: usize) -> RetryError {
    match reason {
        RetryReason::Fatal(err) => RetryError::Fatal(err),
        retryable => RetryError::Exhausted(RetryExhausted {
            attempts,
            last_reason: retryable,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // --- retry(): success after a retry ---

    /// A model call that returns an empty response twice then succeeds is
    /// retried and succeeds — instead of aborting. Pins the retry count too, so
    /// a future change that makes retries infinite is caught.
    #[tokio::test]
    async fn retry_succeeds_after_empty_then_good() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result = retry(
            move || {
                let counter = counter.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(RetryReason::Empty)
                    } else {
                        Ok("ok")
                    }
                }
            },
            RetryPolicy::transient(),
        )
        .await;
        assert_eq!(result.expect("must succeed after retries"), "ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "transient budget is 2: one first attempt plus two retries"
        );
    }

    /// A truncated response (mid-generation truncation) is retried too, and
    /// recovers on the first retry.
    #[tokio::test]
    async fn retry_succeeds_after_truncated_then_good() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result = retry(
            move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(RetryReason::Truncated)
                    } else {
                        Ok(42u8)
                    }
                }
            },
            RetryPolicy::transient(),
        )
        .await;
        assert_eq!(result.expect("must succeed after one retry"), 42u8);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // --- retry(): exhaustion ---

    /// Persistent empty responses exhaust the retry budget and surface
    /// `RetryExhausted { attempts, last_reason }` — the loop stops, it does not
    /// hang or spin forever.
    #[tokio::test]
    async fn retry_exhausts_with_last_reason() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result: Result<(), RetryError> = retry(
            move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(RetryReason::Empty)
                }
            },
            RetryPolicy::transient(),
        )
        .await;
        match result.expect_err("must give up after the retry budget") {
            RetryError::Exhausted(RetryExhausted {
                attempts,
                last_reason,
            }) => {
                assert_eq!(
                    attempts, 2,
                    "exhaustion records the budget number of retries"
                );
                assert!(
                    matches!(last_reason, RetryReason::Empty),
                    "last_reason is the final retryable failure"
                );
            }
            RetryError::Fatal(_) => panic!("budget-starved exhaustion must not be Fatal"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "must stop after one first attempt plus the budget of retries"
        );
    }

    /// A non-retryable reason propagates immediately as `Fatal`, carrying the
    /// original error verbatim — genuine failures (auth, rate limit, network)
    /// are never masked by retries.
    #[tokio::test]
    async fn retry_propagates_fatal_immediately() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result: Result<(), RetryError> = retry(
            move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(RetryReason::Fatal(anyhow::anyhow!("boom")))
                }
            },
            RetryPolicy::transient(),
        )
        .await;
        match result.expect_err("fatal must propagate immediately") {
            RetryError::Fatal(err) => assert_eq!(format!("{err:#}"), "boom"),
            RetryError::Exhausted(_) => panic!("Fatal must not be treated as exhaustion"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "Fatal must not be retried");
    }

    /// `once()` has budget 1: a retryable reason gets exactly one re-roll,
    /// then exhaustion.
    #[tokio::test]
    async fn once_policy_exhausts_after_single_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result: Result<(), RetryError> = retry(
            move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(RetryReason::Markers)
                }
            },
            RetryPolicy::once(),
        )
        .await;
        match result.expect_err("once() budget is 1") {
            RetryError::Exhausted(RetryExhausted {
                attempts,
                last_reason,
            }) => {
                assert_eq!(attempts, 1);
                assert!(matches!(last_reason, RetryReason::Markers));
            }
            RetryError::Fatal(_) => panic!("Markers is retryable, not Fatal"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // --- should_retry(): the budget-gate table ---

    /// The decision spine advances the counter and yields a backoff while the
    /// budget holds, returns `None` exactly when the budget is spent, never
    /// offers a retry for a `Fatal`, and never consumes the budget for one.
    /// Pins the linear backoff steps and the `once()` no-backoff contract too.
    #[test]
    fn should_retry_budget_gate() {
        let empty = RetryReason::Empty;
        let fatal = RetryReason::Fatal(anyhow::anyhow!("network / auth / etc."));

        // transient(): budget 2, linear backoff 300 ms then 600 ms.
        let mut attempts = 0usize;
        assert_eq!(
            should_retry(&empty, &mut attempts, RetryPolicy::transient()),
            Some(Duration::from_millis(300))
        );
        assert_eq!(attempts, 1);
        assert_eq!(
            should_retry(&empty, &mut attempts, RetryPolicy::transient()),
            Some(Duration::from_millis(600))
        );
        assert_eq!(attempts, 2);
        // Budget spent: no more retries, counter unchanged.
        assert_eq!(
            should_retry(&empty, &mut attempts, RetryPolicy::transient()),
            None
        );
        assert_eq!(attempts, 2);

        // Fatal is never retried and never consumes budget, regardless of headroom.
        let mut fresh = 0usize;
        assert_eq!(
            should_retry(&fatal, &mut fresh, RetryPolicy::transient()),
            None
        );
        assert_eq!(fresh, 0, "Fatal must not consume the budget");

        // once(): budget 1, no backoff (retry immediately).
        let mut once = 0usize;
        assert_eq!(
            should_retry(&RetryReason::Markers, &mut once, RetryPolicy::once()),
            Some(Duration::ZERO)
        );
        assert_eq!(once, 1);
        assert_eq!(
            should_retry(&RetryReason::Markers, &mut once, RetryPolicy::once()),
            None
        );
    }
}
