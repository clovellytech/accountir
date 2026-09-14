# Vendored IRS forms

| File | Form | Revision | SHA-256 |
| --- | --- | --- | --- |
| `f1065.pdf` | Form 1065, U.S. Return of Partnership Income | 2025 (rev. 2026-01-08) | `0f19f556e12ef53c41ba27e5930b4373103f3abcf64693c4ea686451b2a8f56f` |
| `f1065sk1.pdf` | Schedule K-1 (Form 1065) | 2025 (rev. 2026-01-06) | `66098d4d48537ce2dac1f093d6351567957896e5843d8c524823b380068f6547` |
| `f1065sb1.pdf` | Schedule B-1, Information on Partners Owning 50% or More | Rev. August 2019 | `0d06ff4c9300381c4fe33688321a9be121738679e2792f9f48d3e8700443db3b` |
| `f1065sb2.pdf` | Schedule B-2, Election Out of the Centralized Partnership Audit Regime | December 2018 | `fe42f9ef2e0901ceaf52c91b262a2a831316fdc807df4e49934705e43fe14eb6` |
| `f4562.pdf` | Form 4562, Depreciation and Amortization | 2025 (rev. 2025-10-09) | `c05f9d1f5e26b1b21e18b0a13adbbcd3d568bb9f3ca53bd7bba7c795c7799b4a` |
| `f1125a.pdf` | Form 1125-A, Cost of Goods Sold | Rev. November 2024 — tax years 2024 onward | `447270f7a058c5b8e3f3e31853bf2bf97470ccc0f5f07359fbcd5ec6a23be5c2` |

## Prior and draft years

The files above are the current year's. The years the Form 1065 page offers are
carried beside them, one directory per tax year:

| File | Form | Revision | SHA-256 |
| --- | --- | --- | --- |
| `2023/f1065.pdf` | Form 1065 | 2023 (rev. 2023-12-13) | `dfbed1281bfbc3ec22291780912d8551c6fcda974bb47a645c69886be8558d36` |
| `2023/f1065sk1.pdf` | Schedule K-1 (Form 1065) | 2023 | `8f517f6c3a7453e1d6b5855c1a4747c58adf1123d6e714460f9eab2664b56122` |
| `2024/f1065.pdf` | Form 1065 | 2024 (rev. 2024-12-05) | `0e090f62c33132f08628c72c964cc7359f4cbbf75581786d1ffb7999ee2f01e4` |
| `2024/f1065sk1.pdf` | Schedule K-1 (Form 1065) | 2024 | `58e55bd504214d6678801fda33917a851fbdf6e26e7140fc9d1bc3d5b7902f9f` |
| `2026/f1065.pdf` | Form 1065 — **DRAFT** | 2026 draft (rev. 2026-07-17) | `7babcb78397fa24d1e4d6176563b628c5214714dad1fd9b7460b5233f9247001` |
| `2026/f1065sk1.pdf` | Schedule K-1 (Form 1065) — **DRAFT** | 2026 draft | `c3e8bc815c3b38cfcd2f583f2b8ee7865d651ff22d012b21ae5f6404a03c5b0a` |
| `2023/f1040sc.pdf` | Schedule C (Form 1040) | 2023 | `d475d0c135c0ebc599eadd1a228cbb124a60b0b6ca69a32ae0d355f5708755ff` |
| `2023/f4562.pdf` | Form 4562 | 2023 | `3df7de3f92fa4cae0c08ad051d0e5da24da4cba48753fb490defc84789428d41` |
| `2023/f1125a.pdf` | Form 1125-A | Rev. November 2018 — tax years through 2023 (`irs-prior/f1125a--2018.pdf`) | `115aa4ff161e36381bfcbff249b5a16df952d934c1491f0548767dc2beb5f314` |
| `2024/f1040sc.pdf` | Schedule C (Form 1040) | 2024 | `f56bfd48f3604fc015b7ea22a70c6c36535a723ba84a2b9a961bc9838d070ce6` |
| `2024/f4562.pdf` | Form 4562 | 2024 | `5a7b8d23cf88e21a57ad7fe3d22294941a8f1161b7a5600ffcd05fa0f6b351a5` |
| `2026/f1040sc.pdf` | Schedule C (Form 1040) — **DRAFT** | 2026 draft | `287b4510dd847c11f6cbbcc98fee05ac5ff187cc18e1413f8759a4a7cd802224` |
| `2026/f4562.pdf` | Form 4562 — **DRAFT** | 2026 draft | `c76264c1ecc0792abc12315eed87802cd934dc18d072c2d72e94180591ca94ef` |

Prior years come from `https://www.irs.gov/pub/irs-prior/<name>--<year>.pdf`,
drafts from `https://www.irs.gov/pub/irs-dft/<name>--dft.pdf`. The 2026 files
are drafts: they exist so the current year can be *projected*, and the IRS says
plainly that a draft may not be filed. Anything produced on them is an estimate
until the final form is published, and the page has to say so.

### Which revisions are actually usable

Carrying a blank is not the same as being able to fill it. Each form's year table
(`FORM_YEARS`, `SCHEDULE_C_YEARS`, `FORM_4562_YEARS`) carries a `mapped` flag, and
an unmapped revision is **refused** rather than filled — because every field name
this program uses still resolves in all of them, which is exactly why they cannot
be trusted. Measured by comparing each box's rectangle against the same box in the
revision this program was written against:

| Form | 2023 | 2024 | 2025 | 2026 draft |
| --- | --- | --- | --- | --- |
| Form 1065 | **own tables** | **own tables** | **own tables** | **refused** — 140 boxes changed row |
| Schedule C | mapped | mapped | mapped | **refused** — expense boxes shift by one, subform renamed |
| Form 4562 | **own table** | **own table** | **own table** | not carried — nothing can reach it |
| Schedule B-1 | rev. 2019 — one revision for every year | | | |
| Schedule B-2 | rev. 2018 — one revision for every year | | | |
| Form 1125-A | rev. 2018 | rev. 2024 | rev. 2024 | rev. 2024 |

A refused Form 4562 is not a refused return: page 1 line 16 comes from the ledger,
not from the form, so the figures are unaffected and the return says the schedule
behind them has to be filled in by hand.

Form 4562 no longer has a `mapped` flag, because it no longer has a reference
revision to be mapped *to*. Each revision carries a complete `Boxes` table naming
only its own PDF, and a year with no table attaches no form. The 2026 draft is not
carried: both the Form 1065 and the Schedule C refuse that year, so nothing can
ask for it, and a table nothing exercises is an assertion nobody checks.

The Form 1065 has no aliases left either. Page one and Schedule B each carry a
complete table per revision, and `apply_aliases` is gone from the codebase. Two
things the alias model could not survive:

- **Page one's income block moves as a unit.** The 2023 and 2024 headers use four
  fewer boxes than 2025's, so gross receipts is `f1_15[0]` on them and `f1_19[0]`
  on 2025 — and on the 2023 form the displacement grows to five below line 20,
  whose energy line is a second widget of `f1_37` rather than a number of its own.
  Filled with the 2025 names, a 2023 return printed gross receipts on line 3 and
  the **ordinary business income on line 28, Total balance due**. All 27 names
  existed on all three forms.
- **Schedule B changes which questions it asks.** The 2023 form has no question
  10e and no 32; it numbers the audit-regime election 31 where the later forms
  number it 33, and asks 13 where they ask 13a. A question a revision does not ask
  now simply has no row in its table — there is no `absent` list, no `numbers`
  override and no `mapped` flag to keep in step with each other. The desktop reads
  the same table, so the screen and the paper cannot disagree about what a
  question is called.

What made the diff model impossible here, concretely: the 2023 and 2024 forms call
the first column of every Section B row `R4[0]`, `R5[0]`, … and the 2025 form calls
it `f1_26[0]`, `f1_32[0]`, … Because the `R*` boxes consume no `f1_` number, every
`f1_` name after them differs by one per row. Part IV's total also moved from page
1 (`f1_108[0]`) to page 2 (`f2_2[0]`). There is no rename table that expresses
either.

### What differs between revisions

Checked by dumping each revision's AcroForm field names and comparing them:

- **Schedule K-1 is stable.** Every field this code names exists, under the same
  name, in the 2023, 2024, 2025 and 2026-draft K-1. No per-year map is needed
  for it.
- **The main form's low field numbers change zero-padding between revisions.**
  2023 and 2025 write `f1_04[0]`, `f5_01[0]`, `f6_01[0]`; 2024 and the 2026
  draft write `f1_4[0]`, `f5_1[0]`, `f6_1[0]`. It alternates, so it cannot be
  predicted from the year — it has to be looked up.
- **Schedule C's lines 27a and 27b swapped numbers in 2025 — and the boxes did
  not move.** The 2023 and 2024 forms print *27a Other expenses (from line 48)*
  above *27b Energy efficient commercial buildings deduction*; the 2025 form
  prints them the other way round. `f1_39[0]` is the other-expenses box and
  `f1_40[0]` the energy box on **every** revision. This module had them
  transposed — both boxes exist, both feed line 28, so the return footed with the
  §179D deduction printed on the other-expenses line. Caught only by
  `a_money_box_sits_on_the_row_its_label_is_on`, which reads each box's rectangle
  against the words printed beside it. A name check cannot see this class of
  defect and never could.
- **Leaf names collide between the two PDFs.** `f1_55[0]` is a Schedule K-1 box
  to `form1065.rs`'s coded-box table and the *bank routing number* on page 1 of
  the 2025 main form. Any comparison of field names has to be done per document,
  or it reports differences that are not there and misses ones that are.

Downloaded from `https://www.irs.gov/pub/irs-pdf/<name>`. Works of the US
federal government, so not under copyright.

Schedule C carries a per-revision `LineBox` table too, though its *boxes* turn
out to be identical on all three revisions — all 105 were compared. What moves is
the printed number: the 2023 and 2024 forms number other-expenses 27a and the
§179D energy deduction 27b, and the 2025 form numbers them the other way round.
The form says so itself, its line 48 reading "enter here and on line 27a" on the
older revisions and "27b" on the current one. The table carries the box as well
as the number so a revision that *does* move one is expressible without another
refactor.

The two B schedules carry their own revision dates rather than a tax year: the
IRS reissues them only when they change, so the 2019 and 2018 revisions are
current for a 2025 return. That also means step 1 below will usually leave them
alone — check `https://www.irs.gov/pub/irs-pdf/f1065sb1.pdf` against the hash
above rather than assuming a new year means a new file.

They are `include_bytes!`d into the binary by `src/tax/form1065.rs`,
`src/tax/schedule_b1.rs`, `src/tax/schedule_b2.rs`, `src/tax/form4562.rs` and
`src/tax/form1125a.rs`.

Form 1125-A is reissued by revision date, like the B schedules, and each
revision serves every tax year until the next. The 2024 revision is not a light
edit of the 2018 one: it drops the cents box beside every amount and adds three
valuation methods and a LIFO reserve line to question 9, renumbering every check
box after 9a. `form1125a::REVISIONS` carries a box table per revision, and
`the_line_boxes_run_down_the_page_in_line_order` holds each to its own PDF. Carried
rather than fetched because a return you can only produce with a working
connection to irs.gov is one you cannot produce on the afternoon it is due.

## Replacing them for a new tax year

1. Download the files over the ones here — all five, though the two B schedules
   usually will not have changed.
2. Update `FORM_TAX_YEAR` in `src/tax/form1065.rs`.
3. Regenerate `docs/form-1065-fields.md` — the field *numbering shifts between
   revisions*, so a constant that named the EIN box last year may name a
   neighbouring one now. Form 4562 has already done this once: the 2025 revision
   inserted **50-year property** at row 19h, pushing residential rental to 19i
   and nonresidential real property to 19j, and every field number after row 19g
   moved with them. A field check cannot catch that — the fields all still exist
   — so read Section B against `src/tax/form4562.rs` by eye.
4. Run `cargo test tax::`. `every_field_this_module_names_exists_in_the_vendored_forms`
   catches a field that has been renamed or removed — as do
   `schedule_b1::every_field_this_module_names_exists_in_the_vendored_schedule`,
   its B-2 twin, and
   `form4562::every_field_this_module_names_exists_in_the_vendored_form` — and
   `the_checkbox_states_are_the_ones_the_form_was_built_with` catches a
   checkbox whose on-state changed. Neither can catch a box that still exists
   under the same name but now means something else — that is what step 3 is
   for, and it needs a person to read it.
