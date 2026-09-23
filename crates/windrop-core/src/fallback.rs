//! Running the fallback chain.
//!
//! A profile offers an ordered list of environments. Each one costs a full
//! prefix creation plus an installer run, so the chain is short and every step
//! differs from the previous one in exactly one dimension — otherwise a success
//! or failure would teach nothing about why.
//!
//! Two rules keep the chain honest:
//!
//! * An error that cannot be fixed by trying a different environment — missing
//!   Wine, missing `winetricks`, an unsupported architecture — aborts
//!   immediately instead of burning three more prefixes.
//! * Variants that previously worked for this application are moved to the
//!   front, so a successful install makes the next one faster.

use crate::compat::profile::RuntimeEnv;
use crate::{Error, Result};

/// What happened to one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    Succeeded,
    /// The variant was tried and failed; the message is user-facing.
    Failed(String),
}

impl AttemptOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, AttemptOutcome::Succeeded)
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            AttemptOutcome::Succeeded => None,
            AttemptOutcome::Failed(message) => Some(message),
        }
    }
}

/// One entry in the attempt log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub variant: RuntimeEnv,
    pub outcome: AttemptOutcome,
}

/// The result of a successful chain run.
#[derive(Debug, Clone)]
pub struct ChainOutcome<T> {
    pub value: T,
    /// The variant that worked.
    pub winning_variant: RuntimeEnv,
    /// Every variant tried, in order, including the successful one.
    pub attempts: Vec<AttemptRecord>,
}

impl<T> ChainOutcome<T> {
    /// How many variants failed before the successful one.
    pub fn retries(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| !a.outcome.is_success())
            .count()
    }

    /// A line for the log pane.
    pub fn summary(&self) -> String {
        match self.retries() {
            0 => format!(
                "succeeded on the first attempt ({})",
                self.winning_variant.rationale
            ),
            n => format!(
                "succeeded after {n} failed attempt{} ({})",
                if n == 1 { "" } else { "s" },
                self.winning_variant.rationale
            ),
        }
    }
}

/// Move a previously successful variant to the front of the chain.
///
/// An unknown signature — a profile that changed, or a first install — leaves
/// the order exactly as the profile specifies.
pub fn order_variants(
    variants: &[RuntimeEnv],
    preferred_signature: Option<&str>,
) -> Vec<RuntimeEnv> {
    let Some(preferred) = preferred_signature else {
        return variants.to_vec();
    };
    let Some(position) = variants.iter().position(|v| v.signature() == preferred) else {
        tracing::debug!(
            "the recorded variant is no longer part of this profile; using the default order"
        );
        return variants.to_vec();
    };
    if position == 0 {
        return variants.to_vec();
    }
    tracing::info!(
        "moving the previously successful variant to the front of the chain (position {})",
        position + 1
    );
    let mut ordered = Vec::with_capacity(variants.len());
    ordered.push(variants[position].clone());
    ordered.extend(
        variants
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != position)
            .map(|(_, v)| v.clone()),
    );
    ordered
}

/// Try each variant until one succeeds.
///
/// `attempt` is called with the variant and its zero-based index, and returns
/// whatever the caller cares about on success. Failures are recorded rather
/// than returned, so a partial log always reaches the caller.
pub fn run_chain<T>(
    app_name: &str,
    variants: &[RuntimeEnv],
    mut attempt: impl FnMut(&RuntimeEnv, usize) -> Result<T>,
) -> Result<ChainOutcome<T>> {
    if variants.is_empty() {
        return Err(Error::Config {
            field: "profile.variants".into(),
            reason: "a profile must offer at least one runtime environment".into(),
        });
    }

    let mut attempts: Vec<AttemptRecord> = Vec::new();
    let mut last_error: Option<String> = None;

    for (index, variant) in variants.iter().enumerate() {
        tracing::info!(
            app = app_name,
            attempt = index + 1,
            of = variants.len(),
            strategy = %variant.rationale,
            "trying compatibility variant"
        );

        match attempt(variant, index) {
            Ok(value) => {
                attempts.push(AttemptRecord {
                    variant: variant.clone(),
                    outcome: AttemptOutcome::Succeeded,
                });
                return Ok(ChainOutcome {
                    value,
                    winning_variant: variant.clone(),
                    attempts,
                });
            }
            Err(e) => {
                let message = e.to_string();
                attempts.push(AttemptRecord {
                    variant: variant.clone(),
                    outcome: AttemptOutcome::Failed(message.clone()),
                });

                if !e.is_recoverable_by_retry() {
                    // Trying other environments cannot fix this, and each
                    // attempt is expensive.
                    tracing::warn!(
                        app = app_name,
                        error = %e,
                        "this failure cannot be resolved by another compatibility variant"
                    );
                    return Err(e);
                }

                if index + 1 == variants.len() {
                    last_error = Some(message);
                    break;
                }

                tracing::warn!(
                    app = app_name,
                    error = %message,
                    "variant failed; trying the next one"
                );
                last_error = Some(message);
            }
        }
    }

    Err(Error::AllVariantsFailed {
        app: app_name.to_string(),
        last_error: last_error.unwrap_or_else(|| "no variants were attempted".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::pe::Arch;
    use crate::compat::profile::WindowsVersion;

    /// Variants that differ in exactly one real dimension each, mirroring how a
    /// chain is built, so their signatures are genuinely distinct.
    fn variant(label: &str) -> RuntimeEnv {
        let (windows_version, dxvk) = match label {
            "first" => (WindowsVersion::Win10, true),
            "second" => (WindowsVersion::Win7, true),
            _ => (WindowsVersion::WinXp, false),
        };
        RuntimeEnv {
            wine_build: "stable".into(),
            arch: Arch::X86_64,
            windows_version,
            dxvk,
            vkd3d_proton: false,
            dll_overrides: vec![],
            env: vec![],
            dependencies: vec![],
            rationale: label.to_string(),
        }
    }

    fn chain() -> Vec<RuntimeEnv> {
        vec![variant("first"), variant("second"), variant("third")]
    }

    /// A failure that the chain should survive.
    fn recoverable(what: &str) -> Error {
        Error::InstallIncomplete {
            rationale: what.to_string(),
        }
    }

    #[test]
    fn the_first_successful_variant_stops_the_chain() {
        let mut calls = 0;
        let outcome = run_chain("app", &chain(), |v, _| {
            calls += 1;
            Ok(v.rationale.clone())
        })
        .unwrap();

        assert_eq!(calls, 1);
        assert_eq!(outcome.value, "first");
        assert_eq!(outcome.winning_variant.rationale, "first");
        assert_eq!(outcome.retries(), 0);
        assert!(outcome.summary().contains("first attempt"));
    }

    #[test]
    fn a_failing_variant_moves_the_chain_on() {
        let outcome = run_chain("app", &chain(), |v, index| {
            if index == 0 {
                Err(recoverable("installer crashed"))
            } else {
                Ok(v.rationale.clone())
            }
        })
        .unwrap();

        assert_eq!(outcome.value, "second");
        assert_eq!(outcome.retries(), 1);
        assert_eq!(outcome.attempts.len(), 2);
        assert_eq!(
            outcome.attempts[0].outcome,
            AttemptOutcome::Failed(
                "the application did not install correctly: installer crashed".into()
            )
        );
        assert!(outcome.attempts[0]
            .outcome
            .message()
            .unwrap()
            .contains("crashed"));
        assert!(outcome.summary().contains("after 1 failed attempt"));
    }

    #[test]
    fn the_last_variant_can_still_win() {
        let outcome = run_chain("app", &chain(), |v, index| {
            if index == 2 {
                Ok(v.rationale.clone())
            } else {
                Err(recoverable("nope"))
            }
        })
        .unwrap();
        assert_eq!(outcome.value, "third");
        assert_eq!(outcome.retries(), 2);
        assert!(outcome.summary().contains("2 failed attempts"));
    }

    #[test]
    fn exhausting_the_chain_reports_the_last_failure() {
        let err = run_chain("Notepad++", &chain(), |_, index| {
            Err::<(), _>(recoverable(&format!("failure {index}")))
        })
        .unwrap_err();

        match err {
            Error::AllVariantsFailed { app, last_error } => {
                assert_eq!(app, "Notepad++");
                assert!(
                    last_error.contains("failure 2"),
                    "the last failure is the useful one"
                );
            }
            other => panic!("expected AllVariantsFailed, got {other:?}"),
        }
    }

    #[test]
    fn an_unfixable_error_aborts_immediately() {
        let mut calls = 0;
        let err = run_chain("app", &chain(), |_, _| {
            calls += 1;
            Err::<(), _>(Error::WinetricksMissing {
                dependency: "vcrun2022".into(),
            })
        })
        .unwrap_err();

        assert_eq!(
            calls, 1,
            "expensive retries must not happen for a missing tool"
        );
        assert!(matches!(err, Error::WinetricksMissing { .. }));
    }

    #[test]
    fn missing_wine_aborts_immediately() {
        let mut calls = 0;
        let err = run_chain("app", &chain(), |_, _| {
            calls += 1;
            Err::<(), _>(Error::WineMissing {
                hint: "install wine".into(),
            })
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(matches!(err, Error::WineMissing { .. }));
    }

    #[test]
    fn an_unsupported_architecture_aborts_immediately() {
        let mut calls = 0;
        let err = run_chain("app", &chain(), |_, _| {
            calls += 1;
            Err::<(), _>(Error::UnsupportedArch("ARM64".into()))
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(matches!(err, Error::UnsupportedArch(_)));
    }

    #[test]
    fn an_empty_chain_is_a_configuration_error() {
        let err = run_chain::<()>("app", &[], |_, _| Ok(())).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
    }

    #[test]
    fn variants_are_passed_their_index() {
        let seen = std::cell::RefCell::new(Vec::new());
        let _ = run_chain("app", &chain(), |v, index| {
            seen.borrow_mut().push((index, v.rationale.clone()));
            if index == 1 {
                Ok(())
            } else {
                Err(recoverable("x"))
            }
        });
        assert_eq!(
            *seen.borrow(),
            vec![(0, "first".to_string()), (1, "second".to_string())]
        );
    }

    #[test]
    fn every_variant_in_the_test_chain_is_distinguishable() {
        let variants = chain();
        let mut signatures: Vec<String> = variants.iter().map(|v| v.signature()).collect();
        let total = signatures.len();
        signatures.sort();
        signatures.dedup();
        assert_eq!(
            signatures.len(),
            total,
            "the fixtures must be distinguishable"
        );
    }

    #[test]
    fn a_learned_variant_is_moved_to_the_front() {
        let variants = chain();
        let preferred = variants[2].signature();
        let ordered = order_variants(&variants, Some(&preferred));
        assert_eq!(
            ordered
                .iter()
                .map(|v| v.rationale.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "first", "second"]
        );
    }

    #[test]
    fn a_first_install_keeps_the_profile_order() {
        let variants = chain();
        let ordered = order_variants(&variants, None);
        assert_eq!(ordered, variants);
    }

    #[test]
    fn a_stale_signature_is_ignored() {
        let variants = chain();
        let ordered = order_variants(&variants, Some("not-a-real-signature"));
        assert_eq!(
            ordered, variants,
            "an unknown signature must not reorder anything"
        );
    }

    #[test]
    fn an_already_first_variant_is_left_alone() {
        let variants = chain();
        let preferred = variants[0].signature();
        assert_eq!(order_variants(&variants, Some(&preferred)), variants);
    }

    #[test]
    fn ordering_preserves_every_variant_exactly_once() {
        let variants = chain();
        let preferred = variants[1].signature();
        let ordered = order_variants(&variants, Some(&preferred));
        assert_eq!(ordered.len(), variants.len());
        for v in &variants {
            assert_eq!(
                ordered
                    .iter()
                    .filter(|o| o.signature() == v.signature())
                    .count(),
                1
            );
        }
    }

    #[test]
    fn a_learned_order_actually_changes_which_variant_is_tried_first() {
        let variants = chain();
        // Previously only the third variant worked.
        let ordered = order_variants(&variants, Some(&variants[2].signature()));
        let mut calls = 0;
        let outcome = run_chain("app", &ordered, |v, _| {
            calls += 1;
            if v.rationale == "third" {
                Ok(())
            } else {
                Err(recoverable("expected only 'third' to work"))
            }
        })
        .unwrap();

        assert_eq!(outcome.value, ());
        assert_eq!(calls, 1, "the learned variant should win on the first try");
    }
}
