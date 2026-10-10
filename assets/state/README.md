# State tax forms

Official fillable PDFs, vendored so a return can be filled offline and so a
form never changes underneath a computation that was checked against it. Each
year is its own folder; a year with no folder is refused, never filled on a
neighbouring year's form (see `tax::md505` and `tax::va763`).

The boxes on these forms are found **by position**, not by field name: the
2025 Maryland forms name some boxes for the wrong line (`43 Enter Dollars 5` is
line 47), and Form 505NR's names carry line breaks. The tests check that every
position each module names lands on a field of the vendored file.

| File | Form | Source | Downloaded |
|---|---|---|---|
| `md/2025/505.pdf` | Maryland Form 505, Nonresident Income Tax Return (COM/RAD-022 09/25) | https://www.marylandcomptroller.gov/content/dam/mdcomp/tax/forms/2025/505.pdf | 2026-10-09 |
| `md/2025/505nr.pdf` | Maryland Form 505NR, Nonresident Income Tax Calculation (COM/RAD-318 09/25) | https://www.marylandcomptroller.gov/content/dam/mdcomp/tax/forms/2025/505nr.pdf | 2026-10-09 |
| `va/2025/763.pdf` | Virginia Form 763, Nonresident Income Tax Return (2601044 Rev. 04/26) | https://www.tax.virginia.gov/sites/default/files/taxforms/individual-income-tax/2025/763-2025.pdf | 2026-10-09 |

Rates, deductions and exemptions were read from the 2025 instructions:
Maryland's nonresident instructions (marylandcomptroller.gov, 2025
`nonresident-booklet.pdf`) and Virginia's Form 763 instructions
(tax.virginia.gov `2025-763-instructions.pdf`).
