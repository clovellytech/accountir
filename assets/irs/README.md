# Vendored IRS forms

| File | Form | Revision | SHA-256 |
| --- | --- | --- | --- |
| `f1065.pdf` | Form 1065, U.S. Return of Partnership Income | 2025 (rev. 2026-01-08) | `0f19f556e12ef53c41ba27e5930b4373103f3abcf64693c4ea686451b2a8f56f` |
| `f1065sk1.pdf` | Schedule K-1 (Form 1065) | 2025 (rev. 2026-01-06) | `66098d4d48537ce2dac1f093d6351567957896e5843d8c524823b380068f6547` |
| `f1065sb1.pdf` | Schedule B-1, Information on Partners Owning 50% or More | Rev. August 2019 | `0d06ff4c9300381c4fe33688321a9be121738679e2792f9f48d3e8700443db3b` |
| `f1065sb2.pdf` | Schedule B-2, Election Out of the Centralized Partnership Audit Regime | December 2018 | `fe42f9ef2e0901ceaf52c91b262a2a831316fdc807df4e49934705e43fe14eb6` |

Downloaded from `https://www.irs.gov/pub/irs-pdf/<name>`. Works of the US
federal government, so not under copyright.

The two B schedules carry their own revision dates rather than a tax year: the
IRS reissues them only when they change, so the 2019 and 2018 revisions are
current for a 2025 return. That also means step 1 below will usually leave them
alone — check `https://www.irs.gov/pub/irs-pdf/f1065sb1.pdf` against the hash
above rather than assuming a new year means a new file.

They are `include_bytes!`d into the binary by `src/tax/form1065.rs`,
`src/tax/schedule_b1.rs` and `src/tax/schedule_b2.rs`. Carried
rather than fetched because a return you can only produce with a working
connection to irs.gov is one you cannot produce on the afternoon it is due.

## Replacing them for a new tax year

1. Download the files over the ones here — all four, though the two B schedules
   usually will not have changed.
2. Update `FORM_TAX_YEAR` in `src/tax/form1065.rs`.
3. Regenerate `docs/form-1065-fields.md` — the field *numbering shifts between
   revisions*, so a constant that named the EIN box last year may name a
   neighbouring one now.
4. Run `cargo test tax::`. `every_field_this_module_names_exists_in_the_vendored_forms`
   catches a field that has been renamed or removed — as do
   `schedule_b1::every_field_this_module_names_exists_in_the_vendored_schedule`
   and its B-2 twin — and
   `the_checkbox_states_are_the_ones_the_form_was_built_with` catches a
   checkbox whose on-state changed. Neither can catch a box that still exists
   under the same name but now means something else — that is what step 3 is
   for, and it needs a person to read it.
