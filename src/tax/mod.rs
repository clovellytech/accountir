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
pub mod form1065;
pub mod lines;
pub mod schedule_b;
pub mod schedule_b1;
pub mod schedule_b2;
pub mod schedule_l;
pub mod statement;

pub use form1065::{Bundle, PartnerFiling, ReturnRequest, build_return, build_return_from_ledger};
pub use lines::{Form1065Lines, MAPPABLE_LINES, TaxLineDef};
pub use schedule_b::{ScheduleB, PARTNERSHIP_REP, QUESTIONS as SCHEDULE_B_QUESTIONS};
pub use schedule_l::ScheduleL;
pub use attachments::{Attachment, Provenance};
