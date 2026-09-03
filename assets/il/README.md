# Vendored Illinois forms

| File | Form | Revision | SHA-256 |
| --- | --- | --- | --- |
| `il1065.pdf` | Form IL-1065, Partnership Replacement Tax Return (+ Illinois Schedule B) | 2025 (R-12/25) | `aa658f2fb42c5b76c7c540323b6566cc5bbe82fce8e3800578ccd527aaab2a0d` |

Downloaded from
`https://tax.illinois.gov/content/dam/soi/en/web/tax/forms/incometax/documents/currentyear/business/partnership/il-1065.pdf`.
A work of the State of Illinois, published for taxpayers to file with; carried in
the binary rather than fetched for the same reason as the IRS forms — a return you
can only produce with a working connection to tax.illinois.gov is one you cannot
produce on the afternoon it is due.

This one PDF is a five-page bundle: Form IL-1065 itself (pages 1–3) and Illinois
**Schedule B**, Partners' or Shareholders' Information (pages 4–5). The 2025
revision "R-12/25" is for tax years ending on or after December 31, 2025 and before
December 31, 2026, which is the same year as the federal `FORM_TAX_YEAR`.

Unlike the IRS forms, this PDF names its fields in plain language — `Ordinary
income/loss`, `Replacement tax`, `Inside/Outside Illinois`, `Schedule B, Section B,
Member 1, Column E - Member's distributable amount of base income or loss` — so the
constants in `src/tax/il1065.rs` read as the form does. It is `include_bytes!`d
there.

## Replacing it for a new tax year

1. Download the file over this one.
2. Run `cargo test tax::il1065`.
   `every_field_this_module_names_exists_in_the_vendored_form` catches a field that
   has been renamed or removed, and `the_checkbox_states_are_the_ones_the_form_was_built_with`
   catches a checkbox whose on-state changed. Neither can catch a box that still
   exists under the same name but now means something else — read the form to be
   sure the step/line labels the constants claim still match.
