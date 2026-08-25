//! Producing tax forms from the books.
//!
//! [`form1065`] builds a US partnership return — Form 1065 with a Schedule K-1
//! per partner — as one PDF whose fields are prefilled but still editable.
//! [`acroform`] is the general machinery underneath it and knows nothing about
//! any particular form.

pub mod acroform;
pub mod form1065;
pub mod lines;

pub use form1065::{Bundle, PartnerFiling, ReturnRequest, build_return, build_return_from_ledger};
pub use lines::{Form1065Lines, MAPPABLE_LINES, TaxLineDef};
