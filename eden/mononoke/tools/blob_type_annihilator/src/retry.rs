/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Bounded retries of idempotent store and metadata operations.

use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use futures_retry::retry;

/// Upper bound on attempts: the backoff doubles its delay with every retry
/// and overflows after about 64 doublings.
pub const MAX_ATTEMPTS: u32 = 30;

/// Longest delay between attempts.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How many times an operation is attempted, and the delay before the
/// first retry.  Each further retry doubles the delay, up to
/// `MAX_RETRY_DELAY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    attempts: usize,
    base_delay: Duration,
}

impl RetryPolicy {
    pub fn new(attempts: u32, base_delay: Duration) -> Result<Self> {
        if !(1..=MAX_ATTEMPTS).contains(&attempts) {
            bail!("attempts must be between 1 and {MAX_ATTEMPTS}, not {attempts}");
        }
        Ok(Self {
            attempts: usize::try_from(attempts)?,
            base_delay: base_delay.min(MAX_RETRY_DELAY),
        })
    }

    /// A single attempt.
    pub fn once() -> Self {
        Self {
            attempts: 1,
            base_delay: Duration::ZERO,
        }
    }

    /// Runs `op` until it succeeds or its attempts are used.  `on_retry`
    /// sees each failure that will be retried, with its attempt number.
    pub async fn run<V, Fut>(
        &self,
        op: impl FnMut(usize) -> Fut + Send,
        mut on_retry: impl FnMut(usize, &anyhow::Error) + Send,
    ) -> Result<V>
    where
        Fut: Future<Output = Result<V>>,
        V: Send + 'static,
    {
        let attempts = self.attempts;
        let (value, _) = retry(op, self.base_delay)
            .binary_exponential_backoff()
            .max_interval(MAX_RETRY_DELAY)
            .max_attempts(attempts)
            // Also called for the final failure, which is not retried.
            .inspect_err(move |attempt, error: &anyhow::Error| {
                if attempt < attempts {
                    on_retry(attempt, error)
                }
            })
            .await
            .with_context(|| format!("failed after {attempts} attempts"))?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use anyhow::anyhow;
    use mononoke_macros::mononoke;

    use super::*;

    /// Runs an operation failing `failures` times under `policy`, returning
    /// its result, how often it ran and which attempts were retried.
    async fn run_failing(
        policy: RetryPolicy,
        failures: usize,
    ) -> (Result<usize>, usize, Vec<usize>) {
        let calls = AtomicUsize::new(0);
        let mut retried = Vec::new();
        let result = policy
            .run(
                |attempt| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call < failures {
                            Err(anyhow!("injected failure"))
                        } else {
                            Ok(attempt)
                        }
                    }
                },
                |attempt, _| retried.push(attempt),
            )
            .await;
        (result, calls.load(Ordering::SeqCst), retried)
    }

    #[mononoke::test]
    async fn failures_within_the_attempts_are_retried() -> Result<()> {
        let policy = RetryPolicy::new(3, Duration::ZERO)?;
        let (result, calls, retried) = run_failing(policy, 2).await;
        assert_eq!(result?, 3, "the third attempt succeeds");
        assert_eq!(calls, 3);
        assert_eq!(retried, vec![1, 2]);
        Ok(())
    }

    #[mononoke::test]
    async fn exhausted_attempts_return_the_last_error_without_a_retry() -> Result<()> {
        let policy = RetryPolicy::new(3, Duration::ZERO)?;
        let (result, calls, retried) = run_failing(policy, 3).await;
        let error = result.expect_err("three failures exhaust three attempts");
        assert!(format!("{error:#}").contains("failed after 3 attempts"));
        assert_eq!(calls, 3);
        assert_eq!(
            retried,
            vec![1, 2],
            "the final failure is not reported as a retry"
        );

        let (result, calls, retried) = run_failing(RetryPolicy::once(), 1).await;
        assert!(result.is_err());
        assert_eq!((calls, retried.len()), (1, 0), "once means no retry");
        Ok(())
    }

    #[mononoke::test]
    fn attempts_are_bounded() {
        assert!(RetryPolicy::new(0, Duration::ZERO).is_err());
        assert!(RetryPolicy::new(MAX_ATTEMPTS, Duration::from_secs(1)).is_ok());
        assert!(
            RetryPolicy::new(MAX_ATTEMPTS + 1, Duration::from_secs(1)).is_err(),
            "the backoff would overflow"
        );
    }
}
