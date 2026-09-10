# Vendored Illinois forms

| File | Form | Revision | SHA-256 |
| --- | --- | --- | --- |
| `il1065.pdf` | Form IL-1065, Partnership Replacement Tax Return (+ Illinois Schedule B) | 2025 (R-12/25) | `aa658f2fb42c5b76c7c540323b6566cc5bbe82fce8e3800578ccd527aaab2a0d` |
| `2023/il1065.pdf` | Form IL-1065 (+ Illinois Schedule B) | 2023 | `111dce3525e236a86bd5a5133f5a0dbecb6c5a71274f8121434caf53b3bea0ac` |
| `2024/il1065.pdf` | Form IL-1065 (+ Illinois Schedule B) | 2024 | `f262831d0de41ec34664877c5941ef38a052791ef72ee21dd670aca69fd04340` |

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

Prior years come from the state's archive, whose path differs from the live site's:
`https://taxarchive.illinois.gov/content/dam/soi/en/web/taxarchive/forms/income-tax/<year>/business/partnership/il-1065.pdf`.
The live site's `.../documents/<year>/...` path — the one search engines report —
returns 404, which is why these were missing.

## Three revisions, and why they can share one box map

`src/tax/il1065.rs` carries a year table (`IL1065_YEARS`), and `build` **refuses**
a year that is not in it. Each blank prints, across its own first page, "This form
is for tax years ending on or after December 31, *year*, and before December 31,
*year+1*" — so a year filled on the wrong blank produces a document that
contradicts itself about which year it is. A test reads that sentence off each
carried blank and checks it against the year the table files it under, so the gate
cannot drift from the paper.

Unlike the IRS forms, **all three revisions share one box map**, and that is a
property of how Illinois names its fields rather than luck. The names are plain
language and carry the line arithmetic with them — `Add L36 - L37`,
`Divide L47 - L50 - a - 1`, `Schedule B, Section B, Member 1, Column E - Member's
distributable amount of base income or loss`. A renumbering would therefore change
the *names*, where it is visible, instead of hiding behind a positional name the
way `f1_19[0]` does on the federal form.

Checked, not assumed: all 227 field names are identical across 2023, 2024 and
2025; the printed line numbers on pages 1–3 match one for one; 2024's boxes are in
exactly the same places as 2025's; and 2023 reflows 78 rows vertically — same
column, same name, a few points up or down, which is a page laid out afresh rather
than a form renumbered. `every_box_this_module_names_exists_in_every_revision`
keeps that true.

The desktop greys the IL-1065 buttons for a year with no carried blank and says
which years it has, rather than failing after the click.

## Replacing it for a new tax year

1. Download the file over this one.
2. Run `cargo test tax::il1065`.
   `every_field_this_module_names_exists_in_the_vendored_form` catches a field that
   has been renamed or removed, and `the_checkbox_states_are_the_ones_the_form_was_built_with`
   catches a checkbox whose on-state changed. Neither can catch a box that still
   exists under the same name but now means something else — read the form to be
   sure the step/line labels the constants claim still match.
