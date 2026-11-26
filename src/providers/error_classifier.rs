use crate::error::GatewayError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Auth,
    RateLimit,
    Transient,
    Permanent,
}

#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub kind: ProviderErrorKind,
    pub status: Option<u16>,
    pub retryable: bool,
    pub lock_status: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct TextRule {
    needles: &'static [&'static str],
    kind: ProviderErrorKind,
    retryable: bool,
    lock_status: Option<u16>,
}

const TEXT_RULES: &[TextRule] = &[
    TextRule {
        needles: &[
            "invalid api key",
            "incorrect api key",
            "unauthorized",
            "forbidden",
            "authentication failed",
            "permission denied",
        ],
        kind: ProviderErrorKind::Auth,
        retryable: true,
        lock_status: Some(401),
    },
    TextRule {
        needles: &[
            "rate limit",
            "rate_limited",
            "too many requests",
            "quota exceeded",
            "insufficient quota",
            "billing quota",
        ],
        kind: ProviderErrorKind::RateLimit,
        retryable: true,
        // 429 is upstream quota pressure, not a dead key — no lock, like 5xx.
        lock_status: None,
    },
    TextRule {
        needles: &[
            "overloaded",
            "capacity",
            "temporarily unavailable",
            "timeout",
            "timed out",
            "connection reset",
            "connection closed",
            "connection refused",
            "dns error",
            "network error",
            "service unavailable",
            "bad gateway",
            "gateway timeout",
            "stream read error",
            "error decoding response body",
            "upstream connection error",
            "upstream connection",
        ],
        kind: ProviderErrorKind::Transient,
        retryable: true,
        lock_status: None, // Transient errors are upstream issues — don't lock the key
    },
];

pub fn classify_provider_error(error: &GatewayError) -> ClassifiedError {
    match error {
        GatewayError::ProviderHttpError { status, body, .. } => classify_http_error(*status, body),
        GatewayError::ProviderError(msg) => classify_text_error(None, msg),
        _ => ClassifiedError::permanent(None),
    }
}

pub fn classify_http_error(status: u16, body: &str) -> ClassifiedError {
    let text_based = classify_text_error(Some(status), body);
    if !matches!(text_based.kind, ProviderErrorKind::Permanent) {
        return text_based;
    }

    match status {
        // Dead/invalid credentials — the only statuses that lock a key.
        401 | 403 => {
            ClassifiedError::retryable(ProviderErrorKind::Auth, Some(status), Some(status))
        }
        // 400/402/407 (request shape / billing, not a dead key) and 429
        // (upstream quota): retryable for failover but never locked —
        // handled like 5xx transient, the key stays available.
        400 | 402 | 407 | 429 => ClassifiedError {
            kind: if status == 429 {
                ProviderErrorKind::RateLimit
            } else {
                ProviderErrorKind::Transient
            },
            status: Some(status),
            retryable: true,
            lock_status: None,
        },
        // 404/410/422 → permanent (invalid request, jangan failover)
        404 | 410 | 422 => ClassifiedError::permanent(Some(status)),
        // 5xx + transient → retryable, but don't lock (upstream issue, not key problem)
        408 | 425 | 500 | 502 | 503 | 504 | 507 | 509 => {
            ClassifiedError {
                kind: ProviderErrorKind::Transient,
                status: Some(status),
                retryable: true,
                lock_status: None, // Don't lock on upstream errors
            }
        }
        // default 4xx lainnya → retryable (failover) tanpa lock, seperti 5xx
        s if (400..500).contains(&s) => ClassifiedError {
            kind: ProviderErrorKind::Transient,
            status: Some(s),
            retryable: true,
            lock_status: None,
        },
        _ => ClassifiedError::permanent(Some(status)),
    }
}

pub fn classify_text_error(status: Option<u16>, body: &str) -> ClassifiedError {
    let lower = body.to_ascii_lowercase();

    for rule in TEXT_RULES {
        if rule.needles.iter().any(|needle| lower.contains(needle)) {
            let lock_status = match (rule.kind, rule.lock_status, status) {
                (ProviderErrorKind::Transient, Some(_), Some(s)) => Some(s),
                (_, lock, _) => lock,
            };
            return ClassifiedError {
                kind: rule.kind,
                status,
                retryable: rule.retryable,
                lock_status,
            };
        }
    }

    ClassifiedError::permanent(status)
}

impl ClassifiedError {
    fn permanent(status: Option<u16>) -> Self {
        Self {
            kind: ProviderErrorKind::Permanent,
            status,
            retryable: false,
            lock_status: None,
        }
    }

    fn retryable(kind: ProviderErrorKind, status: Option<u16>, lock_status: Option<u16>) -> Self {
        Self {
            kind,
            status,
            retryable: true,
            lock_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 401/403 are the only statuses that may lock a key.
    #[test]
    fn lock_only_on_401_403() {
        for status in [400u16, 402, 407, 429, 500, 502, 503, 504] {
            let c = classify_provider_error(&GatewayError::ProviderHttpError {
                status,
                body: String::new(),
                provider: "x".into(),
                key_id: None,
            });
            assert!(
                c.lock_status.is_none(),
                "status {status} must not lock, got lock_status={:?}",
                c.lock_status
            );
        }
        for status in [401u16, 403] {
            let c = classify_provider_error(&GatewayError::ProviderHttpError {
                status,
                body: String::new(),
                provider: "x".into(),
                key_id: None,
            });
            assert_eq!(c.lock_status, Some(status), "status {status} must lock");
        }
    }

    // 429 must stay retryable (failover continues) even though it no longer locks.
    #[test]
    fn rate_limit_stays_retryable_without_lock() {
        let c = classify_provider_error(&GatewayError::ProviderHttpError {
            status: 429,
            body: String::new(),
            provider: "x".into(),
            key_id: None,
        });
        assert!(c.retryable, "429 must remain retryable for failover");
        assert_eq!(c.lock_status, None);

        // Text-matched rate limit too ("Too many requests" body).
        let c2 = classify_text_error(Some(429), "Too many requests, slow down");
        assert_eq!(c2.kind, ProviderErrorKind::RateLimit);
        assert!(c2.retryable);
        assert_eq!(c2.lock_status, None, "text rule rate limit must not lock");
    }

    // 400 stays retryable, no lock.
    #[test]
    fn bad_request_no_lock_retryable() {
        let c = classify_provider_error(&GatewayError::ProviderHttpError {
            status: 400,
            body: String::new(),
            provider: "x".into(),
            key_id: None,
        });
        assert!(c.retryable);
        assert_eq!(c.lock_status, None);
    }

    // Auth text ("unauthorized" etc.) still locks — dead credential.
    #[test]
    fn auth_text_still_locks() {
        let c = classify_text_error(None, "Invalid API key provided");
        assert_eq!(c.kind, ProviderErrorKind::Auth);
        assert_eq!(c.lock_status, Some(401));
    }

    // 404/410/422 remain permanent (no failover, no lock).
    #[test]
    fn not_found_stays_permanent() {
        let c = classify_provider_error(&GatewayError::ProviderHttpError {
            status: 404,
            body: String::new(),
            provider: "x".into(),
            key_id: None,
        });
        assert!(!c.retryable);
        assert_eq!(c.lock_status, None);
    }
}
