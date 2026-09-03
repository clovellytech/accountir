//! Producing tax forms from the books.
//!
//! [`form1065`] builds a US partnership return — Form 1065 with a Schedule K-1
//! per partner — as one PDF whose fields are prefilled but still editable.
//! [`acroform`] is the general machinery underneath it and knows nothing about
//! any particular form. [`schedule_b`] holds the "Other Information" questions,
//! their answers, and the IRS links a preparer needs while answering them.

pub mod acroform;
pub mod attachments;
pub mod allocate;
pub mod constructive;
pub mod depreciation;
pub mod form1065;
pub mod form4562;
pub mod il1065;
pub mod lines;
pub mod schedule_b;
pub mod schedule_b1;
pub mod schedule_b2;
pub mod schedule_l;
pub mod schedule_m;
pub mod statement;

pub use form1065::{
    Bundle, PartnerFiling, ReturnOptions, ReturnRequest, build_return, build_return_from_ledger,
};
pub use lines::{Form1065Lines, MAPPABLE_LINES, TaxLineDef};
pub use schedule_b::{ScheduleB, PARTNERSHIP_REP, QUESTIONS as SCHEDULE_B_QUESTIONS};
pub use schedule_l::ScheduleL;
pub use schedule_m::ScheduleM;
pub use attachments::{Attachment, Provenance};

/// Checking that a warning reads as a sentence, not as source code that leaked.
///
/// # The defect this exists to catch
///
/// Every warning on a return is a long prose string, and in Rust a long prose
/// string is written across several source lines. There are two ways to do that
/// and only one of them is right:
///
/// ```text
/// "the books and the return \
///  disagree"                     → "the books and the return disagree"
///
/// "the books and the return
///  disagree"                     → "the books and the return\n disagree"
/// ```
///
/// The trailing backslash strips the newline *and* the next line's indentation.
/// Without it the literal keeps both, and the warning reaches the reader with a
/// line break and a run of spaces sitting in the middle of a sentence. It has
/// happened fifteen times in this crate, and it is invisible in review: the
/// source looks identical either way at a glance, every test that checks a
/// warning `contains` a phrase still passes, and the damage only shows in the
/// warnings panel where nobody is looking at the code.
///
/// A tool that rewrites these strings can also swallow the backslash and leave
/// the run of spaces on one physical line, which reads the same way to the
/// reader and does not even look wrong in the source. So the check is on the
/// *rendered string*, which is the only place both forms of the defect show up
/// the same way.
///
/// # Why this is a test helper and not a `debug_assert!` in the build
///
/// A warning is produced on the path that generates somebody's tax return, and
/// panicking there over the shape of a message would fail a return for a
/// cosmetic reason — the one moment the message least deserves to be fatal.
/// Failing a test is the right severity.
#[cfg(test)]
pub(crate) mod warning_shape {
    /// What is wrong with this warning, if anything.
    ///
    /// A warning is one paragraph of prose: single spaces, no line breaks, no
    /// tabs, and nothing hanging off either end. Anything else came from the
    /// source and not from the author.
    pub fn problem(warning: &str) -> Option<String> {
        if warning.contains('\n') {
            return Some("contains a line break — the string literal is missing a trailing `\\`".into());
        }
        if warning.contains('\t') {
            return Some("contains a tab".into());
        }
        if warning.contains("  ") {
            return Some(
                "contains a run of spaces mid-sentence — a line continuation was swallowed, \
                 leaving the source line's indentation inside the message"
                    .into(),
            );
        }
        if warning.trim() != warning {
            return Some("has leading or trailing whitespace".into());
        }
        if warning.is_empty() {
            return Some("is empty".into());
        }
        None
    }

    /// Assert every warning in a batch reads properly, naming the offender.
    #[track_caller]
    pub fn assert_all<I, S>(warnings: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut checked = 0;
        for w in warnings {
            let w = w.as_ref();
            checked += 1;
            if let Some(problem) = problem(w) {
                panic!("a warning {problem}:\n  {w:?}");
            }
        }
        assert!(checked > 0, "no warnings were checked — the scenario produced none");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_clean_warning_passes() {
            assert!(problem("Schedule L is blank because nothing was mapped to it.").is_none());
        }

        /// The two shapes the defect actually takes.
        #[test]
        fn both_forms_of_a_missing_continuation_are_caught() {
            // The literal written across lines with no trailing backslash.
            let with_break = "Schedule L is blank because nothing was mapped.\n                 Map your accounts.";
            assert!(problem(with_break).unwrap().contains("line break"));

            // The same literal after a tool swallowed the backslash.
            let with_run = "Schedule L is blank because nothing was mapped.                 Map your accounts.";
            assert!(problem(with_run).unwrap().contains("run of spaces"));
        }

        #[test]
        fn stray_whitespace_at_the_ends_is_caught() {
            assert!(problem(" leading").is_some());
            assert!(problem("trailing ").is_some());
            assert!(problem("").is_some());
        }

        #[test]
        fn an_empty_batch_fails_rather_than_passing_vacuously() {
            let empty: Vec<String> = Vec::new();
            let caught = std::panic::catch_unwind(|| assert_all(&empty));
            assert!(caught.is_err(), "an empty batch must not pass");
        }
    }
}
