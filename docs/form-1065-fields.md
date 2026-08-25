# Form 1065 and Schedule K-1 field names

Generated from the XFA template inside the vendored IRS PDFs in `assets/irs/`.

The AcroForm field names are opaque (`f1_14`), so this table is what makes
`src/tax/form1065.rs` reviewable: it is the evidence that a constant names the
box a reader thinks it names.

Regenerate after replacing a form with a new revision — the numbering shifts
between tax years, and a stale constant fills the wrong box silently.


## Form 1065 (f1065.pdf) — 440 fields

| Field | Description |
| --- | --- |
| `f1_01` | Page 1. For calendar year 2025, or tax year beginning. Month and day. |
| `f1_02` | , 2025, ending. Month and day. |
| `f1_03` | , 20. 2 digit year. |
| `f1_04` | Name of partnership. |
| `f1_05` | Number and street. |
| `f1_06` | Room or suite no. |
| `f1_07` | City or town. |
| `f1_08` | State or province. |
| `f1_09` | Country. |
| `f1_10` | Z I P or foreign postal code. |
| `f1_11` | A. Principal business activity. |
| `f1_12` | B. Principal product or service. |
| `f1_13` | C. Business code number. |
| `f1_14` | D. Employer identification number. |
| `f1_15` | E. Date business started. |
| `f1_16` | F. Total assets (see instructions). $. |
| `c1_1` | G. Check applicable boxes: (1). Initial return. |
| `c1_2` | G. (2). Final return. |
| `c1_3` | G. (3). Name change. |
| `c1_4` | G. (4). Address change. |
| `c1_5` | G. (5). Amended return. |
| `c1_6` | H. Check accounting method: (1). Cash. |
| `c1_6` | H. (2). Accrual. |
| `c1_6` | H. (3). Other. |
| `f1_17` | H. (3). Other (specify): |
| `f1_18` | I. Number of Schedules K-1. Attach one for each person who was a partner at any time during the tax year: |
| `c1_7` | J. Check if Schedules C and M-3 are attached. |
| `c1_8` | K. Check if partnership: (1). Aggregated activities for section 465 at-risk purposes. |
| `c1_9` | K. (2). Grouped activities for section 469 passive activity purposes. |
| `f1_19` | Caution: Include only trade or business income and expenses on lines 1a through 23 below. See instructions for more information. Income. 1a. Gross receipts or sales. |
| `f1_20` | 1b. Less returns and allowances. |
| `f1_21` | 1c. Balance. |
| `f1_22` | 2. Cost of goods sold (attach Form 1125-A). |
| `f1_23` | 3. Gross profit. Subtract line 2 from line 1c. |
| `f1_24` | 4. Ordinary income (loss) from other partnerships, estates, and trusts (attach statement). |
| `f1_25` | 5. Net farm profit (loss) (attach Schedule F (Form 1040)). |
| `f1_26` | 6. Net gain (loss) from Form 4797, Part I I, line 17 (attach Form 4797). |
| `f1_27` | 7. Other income (loss) (attach statement). |
| `f1_28` | 8. Total income (loss). Combine lines 3 through 7. |
| `f1_29` | Deductions (see instructions for limitations). 9. Salaries and wages (other than to partners) (less employment credits). |
| `f1_30` | 10. Guaranteed payments to partners. |
| `f1_31` | 11. Repairs and maintenance. |
| `f1_32` | 12. Bad debts. |
| `f1_33` | 13. Rent. |
| `f1_34` | 14. Taxes and licenses. |
| `f1_35` | 15. Interest (see instructions). |
| `f1_36` | 16a. Depreciation (if required, attach Form 4562). |
| `f1_37` | 16b. Less depreciation reported on Form 1125-A and elsewhere on return. |
| `f1_38` | 16c. Amount. |
| `f1_39` | 17. Depletion (Do not deduct oil and gas depletion.). |
| `f1_40` | 18. Retirement plans, etc. |
| `f1_41` | 19. Employee benefit programs. |
| `f1_42` | 20. Energy efficient commercial buildings deduction (attach Form 7205). |
| `f1_43` | 21. Other deductions (attach statement). |
| `f1_44` | 22. Total deductions. Add the amounts shown in the far right column for lines 9 through 21. |
| `f1_45` | 23. Ordinary business income (loss). Subtract line 22 from line 8. |
| `f1_46` | Tax and Payment. 24. Interest due under the look-back method—completed long-term contracts (attach Form 8697). |
| `f1_47` | 25. Interest due under the look-back method—income forecast method (attach Form 8866). |
| `f1_48` | 26. B B A A A R imputed underpayment (see instructions). |
| `f1_49` | 27. Other taxes (see instructions). |
| `f1_50` | 28. Total balance due. Add lines 24 through 27. |
| `f1_51` | 29. Elective payment election amount from Form 3800. |
| `f1_52` | 30. Payment (see instructions). |
| `f1_53` | 31. Amount owed. If the sum of line 29 and line 30 is smaller than line 28, enter amount owed. |
| `f1_54` | 32a. Overpayment. If the sum of line 29 and line 30 is larger than line 28, enter overpayment. |
| `f1_55` | 32b. Routing number. |
| `c1_10` | 32c. Type: Checking. |
| `c1_10` | 32c. Savings. |
| `f1_56` | 32d. Account number. |
| `c1_11` | Sign Here. May the I R S discuss this return with the preparer shown below? See instructions. Yes. |
| `c1_11` | No. |
| `f1_57` | Paid Preparer Use Only. Enter preparer's name. |
| `c1_12` | Check if self-employed. |
| `f1_58` | P T I N. |
| `f1_59` | Firm’s name. |
| `f1_60` | Firm's E I N. |
| `f1_61` | Firm’s address. |
| `f1_62` | Phone no. |
| `c2_1` | Page 2. Schedule B. Other Information. 1. What type of entity is filing this return? Check the applicable box: a. Domestic general partnership. |
| `c2_1` | 1b. Domestic limited partnership. |
| `c2_1` | 1c. Domestic limited liability company. |
| `c2_1` | 1d. Domestic limited liability partnership. |
| `c2_1` | 1e. Foreign partnership. |
| `c2_1` | 1f. Other: . |
| `f2_01` | 1f. Other: |
| `c2_2` | 2. At the end of the tax year: a. Did any foreign or domestic corporation, partnership (including any entity treated as a partnership), trust, or tax-exempt organization, or any foreign government own, directly or indirectly, an interest of 50% or more in the profit, loss, or capital of the partnership? For rules of constructive ownership, see instructions. If “Yes,” attach Schedule B-1, Information on Partners Owning 50% or More of the Partnership. Yes. |
| `c2_2` | 2a. No. |
| `c2_3` | 2b. Did any individual or estate own, directly or indirectly, an interest of 50% or more in the profit, loss, or capital of the partnership? For rules of constructive ownership, see instructions. If “Yes,” attach Schedule B-1. Yes. |
| `c2_3` | 2b. No. |
| `c2_4` | 3. At the end of the tax year, did the partnership: a. Own directly 20% or more, or own, directly or indirectly, 50% or more of the total voting power of all classes of stock entitled to vote of any foreign or domestic corporation? For rules of constructive ownership, see instructions. If “Yes,” complete (i) through (i v) below. Yes. |
| `c2_4` | 3a. No. |
| `f2_02` | Row: 1. Column: (i) Name of corporation. |
| `f2_03` | Row: 1. Column: (i i) Employer identification number (if any). |
| `f2_04` | Row: 1. Column: (i i i) Country of incorporation. |
| `f2_05` | Row: 1. Column: (i v) Percentage owned in voting stock. |
| `f2_06` | Row: 2. Column: (i) Name of corporation. |
| `f2_07` | Row: 2. Column: (i i) Employer identification number (if any). |
| `f2_08` | Row: 2. Column: (i i i) Country of incorporation. |
| `f2_09` | Row: 2. Column: (i v) Percentage owned in voting stock. |
| `f2_10` | Row: 3. Column: (i) Name of corporation. |
| `f2_11` | Row: 3. Column: (i i) Employer identification number (if any). |
| `f2_12` | Row: 3. Column: (i i i) Country of incorporation. |
| `f2_13` | Row: 3. Column: (i v) Percentage owned in voting stock. |
| `f2_14` | Row: 4. Column: (i) Name of corporation. |
| `f2_15` | Row: 4. Column: (i i) Employer identification number (if any). |
| `f2_16` | Row: 4. Column: (i i i) Country of incorporation. |
| `f2_17` | Row: 4. Column: (i v) Percentage owned in voting stock. |
| `f2_18` | Row: 5. Column: (i) Name of corporation. |
| `f2_19` | Row: 5. Column: (i i) Employer identification number (if any). |
| `f2_20` | Row: 5. Column: (i i i) Country of incorporation. |
| `f2_21` | Row: 5. Column: (i v) Percentage owned in voting stock. |
| `c2_5` | 3b. Own directly an interest of 20% or more, or own, directly or indirectly, an interest of 50% or more, in the profit, loss, or capital in any foreign or domestic partnership (including an entity treated as a partnership) or in the beneficial interest of a trust? For rules of constructive ownership, see instructions. If “Yes,” complete (i) through (v) below. Yes. |
| `c2_5` | 3b. No. |
| `f2_22` | Row: 1. Column: (i) Name of entity. |
| `f2_23` | Row: 1. Column: (i i) Employer identification number (if any). |
| `f2_24` | Row: 1. Column: (i i i) Type of entity. |
| `f2_25` | Row: 1. Column: (i v) Country of organization. |
| `f2_26` | Row: 1. Column: (v) Maximum percentage owned in profit, loss, or capital. |
| `f2_27` | Row: 2. Column: (i) Name of entity. |
| `f2_28` | Row: 2. Column: (i i) Employer identification number (if any). |
| `f2_29` | Row: 2. Column: (i i i) Type of entity. |
| `f2_30` | Row: 2. Column: (i v) Country of organization. |
| `f2_31` | Row: 2. Column: (v) Maximum percentage owned in profit, loss, or capital. |
| `f2_32` | Row: 3. Column: (i) Name of entity. |
| `f2_33` | Row: 3. Column: (i i) Employer identification number (if any). |
| `f2_34` | Row: 3. Column: (i i i) Type of entity. |
| `f2_35` | Row: 3. Column: (i v) Country of organization. |
| `f2_36` | Row: 3. Column: (v) Maximum percentage owned in profit, loss, or capital. |
| `f2_37` | Row: 4. Column: (i) Name of entity. |
| `f2_38` | Row: 4. Column: (i i) Employer identification number (if any). |
| `f2_39` | Row: 4. Column: (i i i) Type of Entity. |
| `f2_40` | Row: 4. Column: (i v) Country of organization. |
| `f2_41` | Row: 4. Column: (v) Maximum percentage owned in profit, loss, or capital. |
| `f2_42` | Row: 5. Column: (i) Name of entity. |
| `f2_43` | Row: 5. Column: (i i) Employer identification number (if any). |
| `f2_44` | Row: 5. Column: (i i i) Type of entity. |
| `f2_45` | Row: 5. Column: (i v)Country of organization. |
| `f2_46` | Row: 5. Column: (v) Maximum percentage owned in profit, loss, or capital. |
| `c2_6` | 4. Does the partnership satisfy all four of the following conditions? a. The partnership's total receipts for the tax year were less than $250,000. 4b. The partnership's total assets at the end of the tax year were less than $1 million. 4c. Schedules K-1 are filed with the return and furnished to the partners on or before the due date (including extensions) for the partnership return. 4d. The partnership is not filing and is not required to file Schedule M-3. If "Yes," the partnership is not required to complete Schedules L, M-1, and M-2; item F on page 1 of Form 1065; or item L on Schedule K-1. Yes. |
| `c2_6` | 4. No. |
| `c2_7` | 5. Is this partnership a publicly traded partnership, as defined in section 469(k)(2)? Yes. |
| `c2_7` | 5. No. |
| `c2_8` | 6. During the tax year, did the partnership have any debt that was canceled, was forgiven, or had the terms modified so as to reduce the principal amount of the debt? Yes. |
| `c2_8` | 6. No. |
| `c2_9` | 7. Has this partnership filed, or is it required to file, Form 8918, Material Advisor Disclosure Statement, to provide information on any reportable transaction? Yes. |
| `c2_9` | 7. No. |
| `f2_47` | 8. At any time during calendar year 2025, did the partnership have an interest in or a signature or other authority over a financial account in a foreign country (such as a bank account, securities account, or other financial account)? See instructions for exceptions and filing requirements for F i n C E N Form 114, Report of Foreign Bank and Financial Accounts (F B A R). If “Yes,” enter the name of the foreign country. |
| `c2_10` | 8. Yes. |
| `c2_10` | 8. No. |
| `c2_11` | 9. At any time during the tax year, did the partnership receive a distribution from, or was it the grantor of, or transferor to, a foreign trust? If “Yes,” the partnership may have to file Form 3520, Annual Return To Report Transactions With Foreign Trusts and Receipt of Certain Foreign Gifts. See instructions. Yes. |
| `c2_11` | 9. No. |
| `f2_48` | 10a. Is the partnership making, or had it previously made (and not revoked), a section 754 election? If “Yes” enter the effective date of the election. |
| `c2_12` | 10a. Yes. |
| `c2_12` | 10a. No. |
| `f2_49` | 10b. For this tax year did the partnership make an optional basis adjustment under section 743(b)? If "Yes" enter the total aggregate net positive amount. |
| `f2_50` | 10b. and the total aggregate net negative amount. $. |
| `c2_13` | 10b. of such section 743(b) adjustments for all partners made in the tax year. The partnership must also attach a statement showing the computation and allocation of each basis adjustment. See instructions. Yes |
| `c2_13` | 10b. No. |
| `f3_1` | Page 3. Schedule B. Other Information (continued). 10c. For this tax year did the partnership make an optional basis adjustment under section 734(b)? If "Yes" enter the total aggregate net positive amount $. |
| `f3_2` | 10b. and the total aggregate net negative amount. $. |
| `c3_1` | of such section 734(b) adjustments for all partnership property made in the tax year. The partnership must also attach a statement showing the computation and allocation of each basis adjustment. See instructions. Yes. |
| `c3_1` | 10c. No. |
| `f3_3` | 10d. For this tax year, is the partnership required to adjust the basis of partnership property under section 743(b) or 734(b) because of a substantial built-in loss (as defined under section 743(d)) or substantial basis reduction (as defined under section 734(d))? If “Yes,” enter the total aggregate amount of such section 743(b) adjustments and/or section 734(b) adjustments for all partners and/or partnership property made in the tax year. $. |
| `c3_2` | 10d. The partnership must also attach a statement showing the computation and allocation of the basis adjustment. See instructions. Yes. |
| `c3_2` | 10d. No. |
| `c3_3` | 10e. Reserved for future use. Yes. |
| `c3_3` | 10e. No. |
| `c3_4` | 11. Check this box if, during the current or prior tax year, the partnership distributed any property received in a like-kind exchange or contributed such property to another entity (other than disregarded entities wholly owned by the partnership throughout the tax year). |
| `c3_5` | 12. At any time during the tax year, did the partnership distribute to any partner a tenancy-in-common or other undivided interest in partnership property? Yes. |
| `c3_5` | 12. No. |
| `f3_4` | 13a. If the partnership is required to file Form 8858, Information Return of U.S. Persons With Respect to Foreign Disregarded Entities (F D Es) and Foreign Branches (F Bs), enter the number of Forms 8858 attached. See instructions. |
| `f3_5` | 14. Does the partnership have any foreign partners? If “Yes,” enter the number of Forms 8805, Foreign Partner’s Information Statement of Section 1446 Withholding Tax, filed for this partnership. |
| `c3_6` | 14. Yes. |
| `c3_6` | 14. No. |
| `f3_6` | 15. Enter the number of Forms 8865, Return of U.S. Persons With Respect to Certain Foreign Partnerships, attached to this return. |
| `c3_7` | 16a. Did you make any payments in 2025 that would require you to file Form(s) 1099? See instructions. Yes. |
| `c3_7` | 16a. No. |
| `c3_8` | 16b. If "Yes," did you or will you file required Form(s) 1099? Yes. |
| `c3_8` | 16b. No. |
| `f3_7` | 17. Enter the number of Forms 5471, Information Return of U.S. Persons With Respect to Certain Foreign Corporations, attached to this return. |
| `f3_8` | 18. Enter the number of partners that are foreign governments under section 892. |
| `c3_9` | 19. During the partnership’s tax year, did the partnership make any payments, or receive any payments allocable to foreign partners, that would require it to file Forms 1042 and 1042-S under chapter 3 (sections 1441 through 1464) or chapter 4 (sections 1471 through 1474)? Yes. |
| `c3_9` | 19. No. |
| `c3_10` | 20. Was the partnership a specified domestic entity required to file Form 8938 for the tax year? See the Instructions for Form 8938. Yes. |
| `c3_10` | 20. No. |
| `c3_11` | 21. Is the partnership a section 721(c) partnership, as defined in Regulations section 1.721(c)-1(b)(14)? Yes. |
| `c3_11` | 21. No. |
| `c3_12` | 22. During the tax year, did the partnership pay or accrue any interest or royalty for which one or more partners are not allowed a deduction under section 267A? See instructions. Yes. |
| `c3_12` | 22. No. |
| `f3_9` | 22. If “Yes,” enter the total amount of the disallowed deductions. $. |
| `c3_13` | 23. Did the partnership have an election under section 163(j) for any real property trade or business or any farming business in effect during the tax year? See instructions. Yes. |
| `c3_13` | 23. No. |
| `c3_14` | 24. Does the partnership satisfy one or more of the following? See instructions: a. The partnership owns a pass-through entity with current, or prior year carryover, excess business interest expense. b. The partnership’s aggregate average annual gross receipts (determined under section 448(c)) for the 3 tax years preceding the current tax year are more than $31 million and the partnership has business interest expense. c. The partnership is a tax shelter (see instructions) and the partnership has business interest expense. If “Yes” to any, complete and attach Form 8990. Yes. |
| `c3_14` | 24. No. |
| `c3_15` | 25. Does the partnership intend to self-certify as a qualified opportunity fund? Yes. |
| `c3_15` | 25. No. |
| `f3_10` | 25. If “Yes,” complete and attach Form 8996, Qualified Opportunity Fund, and enter the amount (if any) from Form 8996, line 15. $. |
| `f3_11` | 26. Enter the number of foreign partners subject to section 864(c)(8) as a result of transferring all or a portion of an interest in the partnership or of receiving a distribution from the partnership. |
| `c3_16` | 27. At any time during the tax year, were there any transfers between the partnership and its partners subject to the disclosure requirements of Regulations section 1.707-8? Yes. |
| `c3_16` | 27. No. |
| `f4_01` | Page 4. Schedule B. Other Information (continued). 28. Since December 22, 2017, did a foreign corporation directly or indirectly acquire substantially all of the properties constituting a trade or business of your partnership, and was the ownership percentage (by vote or value) for purposes of section 7874 greater than 50% (for example, the partners held more than 50% of the stock of the foreign corporation)? If “Yes,” list the ownership percentage by vote and by value. See instructions. Percentage: By vote: |
| `f4_02` | 28. By value: |
| `c4_1` | 28. Yes. |
| `c4_1` | 28. No. |
| `c4_2` | 29. Is the partnership required to file Form 7208, Excise Tax on Repurchase of Corporate Stock (see instructions): a. Under the applicable foreign corporation rules? Yes. |
| `c4_2` | 29a. No. |
| `c4_3` | 29b. Under the covered surrogate foreign corporation rules? Yes. If “Yes” to either (a) or (b), complete Form 7208. See the Instructions for Form 7208. |
| `c4_3` | 29b. No. |
| `c4_4` | 30. At any time during this tax year, did the partnership (a) receive (as a reward, award, or payment for property or services); or (b) sell, exchange, or otherwise dispose of a digital asset (or financial interest in a digital asset)? See instructions. Yes. |
| `c4_4` | 30. No. |
| `c4_5` | 32. Check this box if an election out of subchapter K under section 761 is being made. See instructions. |
| `c4_6` | 31. Is the partnership electing out of the centralized partnership audit regime under section 6221(b)? See instructions. Yes. |
| `c4_6` | 31. If “No,” complete Designation of Partnership Representative below. No. |
| `f4_03` | 31. If "Yes" the partnership must complete Schedule B-2 (Form 1065). Enter the total from Schedule B-2 Part I I I line 3. |
| `f4_04` | Designation of Partnership Representative (see instructions). Enter below the information for the partnership representative (P R) for the tax year covered by this return. First name of P R (or entity name). |
| `f4_05` | Last name of P R. |
| `f4_06` | U.S. address of P R. Street. |
| `f4_07` | City. |
| `f4_08` | State. |
| `f4_09` | Z I P code. |
| `f4_10` | U.S. phone number of P R. |
| `f4_11` | Name of designated individual (D I) if P R is an entity. First name of D I. |
| `f4_12` | Last name of D I. |
| `f4_13` | U.S. address of D I. Street. |
| `f4_14` | City. |
| `f4_15` | State. |
| `f4_16` | Z I P code. |
| `f4_17` | U.S. phone number of D I. |
| `f5_01` | Page 5. Schedule K. Partners’ Distributive Share Items. Income (Loss). 1. Ordinary business income (loss) (page 1, line 23). Total amount. |
| `f5_02` | 2. Net rental real estate income (loss) (attach Form 8825). |
| `f5_03` | 3a. Other gross rental income (loss). |
| `f5_04` | 3b. Expenses from other rental activities (attach statement). |
| `f5_05` | 3c. Other net rental income (loss). Subtract line 3b from line 3a. |
| `f5_06` | 4. Guaranteed payments. a. Services. |
| `f5_07` | 4b. Capital. |
| `f5_08` | 4c. Total. Add lines 4a and 4b. |
| `f5_09` | 5. Interest income. |
| `f5_10` | 6. Dividends and dividend equivalents: a. Ordinary dividends. |
| `f5_11` | 6b. Qualified Dividends. |
| `f5_12` | 6c. Dividend equivalents. |
| `f5_13` | 7. Royalties. |
| `f5_14` | 8. Net short-term capital gain (loss) (attach Schedule D (Form 1065)). |
| `f5_15` | 9a. Net long-term capital gain (loss) (attach Schedule D (Form 1065)). |
| `f5_16` | 9b. Collectibles (28%) gain (loss). |
| `f5_17` | 9c. Unrecaptured section 1250 gain (attach statement). |
| `f5_18` | 10. Net section 1231 gain (loss) (attach Form 4797). |
| `f5_19` | 11. Other income (loss) (see instructions). Type: |
| `f5_20` | 11. Amount. |
| `f5_21` | Deductions. 12. Section 179 deduction (attach Form 4562). |
| `f5_22` | 13a. Cash contributions. |
| `f5_23` | 13b. Noncash contributions. |
| `f5_24` | 13c. Investment interest expense. |
| `f5_25` | 13d. Section 59(e)(2) expenditures: (1) Type: |
| `f5_26` | 13d. (2) Amount: . |
| `f5_27` | 13e. Other deductions (see instructions). Type: |
| `f5_28` | 13e. Amount. |
| `f5_29` | Self-Employment. 14a. Net earnings (loss) from self-employment. |
| `f5_30` | 14b. Gross farming or fishing income. |
| `f5_31` | 14c. Gross nonfarm income. |
| `f5_32` | Credits. 15a. Low-income housing credit (section 42(j)(5)). |
| `f5_33` | 15b. Low-income housing credit (other). |
| `f5_34` | 15c. Qualified rehabilitation expenditures (rental real estate) (attach Form 3468, if applicable). |
| `f5_35` | 15d. Other rental real estate credits (see instructions). Type: |
| `f5_36` | 15d. Amount. |
| `f5_37` | 15e. Other rental credits (see instructions). Type: |
| `f5_38` | 15e. Amount. |
| `f5_39` | 15f. Other credits (see instructions). Type: |
| `f5_40` | 15f. Amount. |
| `c5_1` | International. 16a. Attach Schedule K-2 (Form 1065), Partners' Distributive Share Items—International, and check this box to indicate that you are reporting items of international tax relevance. |
| `c5_2` | 16b. Check this box if you qualified for an exception to filing Schedule K-2 (Form 1065). |
| `f5_41` | Alternative Minimum Tax (A M T) Items. 17a. Post-1986 depreciation adjustment. |
| `f5_42` | 17b. Adjusted gain or loss. |
| `f5_43` | 17c. Depletion (other than oil and gas). |
| `f5_44` | 17d. Oil, gas, and geothermal properties–gross income. |
| `f5_45` | 17e. Oil, gas, and geothermal properties–deductions. |
| `f5_46` | 17f. Other A M T items (attach statement). |
| `f5_47` | Other Information. 18a. Tax-exempt interest income. |
| `f5_48` | 18b. Other tax-exempt income. |
| `f5_49` | 18c. Nondeductible expenses. |
| `f5_50` | 19a. Distributions of cash and marketable securities. |
| `f5_51` | 19b. Distributions of other property. |
| `f5_52` | 20a. Investment income. |
| `f5_53` | 20b. Investment expenses. |
| `f5_54` | 20c. Other items and amounts (attach statement). |
| `f5_55` | 21. Total foreign taxes paid or accrued. |
| `f6_01` | Page 6. Analysis of Net Income (Loss) per Return. 1. Net income (loss). Combine Schedule K, lines 1 through 11. From the result, subtract the sum of Schedule K, lines 12 through 13e, and 21. |
| `f6_02` | 2. Analysis by partner type: Row: a. General partners. Column: (i) Corporate. |
| `f6_03` | Row: 2a. General partners. Column: (i i) Individual (active). |
| `f6_04` | Row: 2a. General partners. Column: (i i i) Individual (passive). |
| `f6_05` | Row: 2a. General partners. Column: (i v) Partnership. |
| `f6_06` | Row: 2a. General partners. Column: (v) Exempt organization. |
| `f6_07` | Row: 2a. General partners. Column: (v i) Nominee/Other. |
| `f6_08` | Row: 2b. Limited partners. Column: (i) Corporate. |
| `f6_09` | Row: 2b. Limited partners. Column: (i i) Individual (active). |
| `f6_10` | Row: 2b. Limited partners. Column: (i i i) Individual (passive). |
| `f6_11` | Row: 2b. Limited partners. Column: (i v) Partnership. |
| `f6_12` | Row: 2b. Limited partners. Column: (v) Exempt organization. |
| `f6_13` | Row: 2b. Limited partners. Column: (v i) Nominee/Other. |
| `f6_14` | Schedule L. Balance Sheets per Books. Assets. Row: 1. Cash. Column: Beginning of tax year. (a). |
| `f6_15` | Row: 1. Cash. Column: Beginning of tax year. (b). |
| `f6_16` | Row: 1. Cash. Column: End of tax year. (c). |
| `f6_17` | Row: 1. Cash. Column: End of tax year. (d). |
| `f6_18` | Row: 2a. Trade notes and accounts receivable. Column: Beginning of tax year. (a). |
| `f6_19` | Row: 2a. Trade notes and accounts receivable. Column: Beginning of tax year. (b). |
| `f6_20` | Row: 2a. Trade notes and accounts receivable. Column: End of tax year. (c). |
| `f6_21` | Row: 2a. Trade notes and accounts receivable. Column: End of tax year. (d). |
| `f6_22` | Row: 2b. Less allowance for bad debts. Column: Beginning of tax year. (a). |
| `f6_23` | Row: 2b. Less allowance for bad debts. Column: Beginning of tax year. (b). |
| `f6_24` | Row: 2b. Less allowance for bad debts. Column: End of tax year. (c). |
| `f6_25` | Row: 2b. Less allowance for bad debts. Column: End of tax year. (d). |
| `f6_26` | Row: 3. Inventories. Column: Beginning of tax year. (a). |
| `f6_27` | Row: 3. Inventories. Column: Beginning of tax year. (b). |
| `f6_28` | Row: 3. Inventories. Column: End of tax year. (c). |
| `f6_29` | Row: 3. Inventories. Column: End of tax year. (d). |
| `f6_30` | Row: 4. U.S. Government obligations. Column: Beginning of tax year. (a). |
| `f6_31` | Row: 4. U.S. Government obligations. Column: Beginning of tax year. (b). |
| `f6_32` | Row: 4. U.S. Government obligations. Column: End of tax year. (c). |
| `f6_33` | Row: 4. U.S. Government obligations. Column: End of tax year. (d). |
| `f6_34` | Row: 5. Tax-exempt securities. Column: Beginning of tax year. (a). |
| `f6_35` | Row: 5. Tax-exempt securities. Column: Beginning of tax year. (b). |
| `f6_36` | Row: 5. Tax-exempt securities. Column: End of tax year. (c). |
| `f6_37` | Row: 5. Tax-exempt securities. Column: End of tax year. (d). |
| `f6_38` | Row: 6. Other current assets (attach statement). Column: Beginning of tax year. (a). |
| `f6_39` | Row: 6. Other current assets (attach statement). Column: Beginning of tax year. (b). |
| `f6_40` | Row: 6. Other current assets (attach statement). Column: End of tax year year. (c). |
| `f6_41` | Row: 6. Other current assets (attach statement). Column: End of tax year. (d). |
| `f6_42` | Row: 7a. Loans to partners (or persons related to partners). Column: Beginning of tax year. (a). |
| `f6_43` | Row: 7a. Loans to partners (or persons related to partners). Column: Beginning of tax year. (b). |
| `f6_44` | Row: 7a. Loans to partners (or persons related to partners). Column: End of tax year. (c). |
| `f6_45` | Row: 7a. Loans to partners (or persons related to partners). Column: End of tax year. (d). |
| `f6_46` | Row: 7b. Mortgage and real estate loans. Column: Beginning of tax year. (a). |
| `f6_47` | Row: 7b. Mortgage and real estate loans. Column: Beginning of tax year. (b). |
| `f6_48` | Row: 7b. Mortgage and real estate loans. Column: End of tax year. (c). |
| `f6_49` | Row: 7b. Mortgage and real estate loans. Column: End of tax year. (d). |
| `f6_50` | Row: 8. Other investments (attach statement). Column: Beginning of tax year. (a). |
| `f6_51` | Row: 8. Other investments (attach statement). Column: Beginning of tax year. (b). |
| `f6_52` | Row: 8. Other investments (attach statement). Column: End of tax year. (c). |
| `f6_53` | Row: 8. Other investments (attach statement). Column: End of tax year. (d). |
| `f6_54` | Row: 9a. Buildings and other depreciable assets. Column: Beginning of tax year. (a). |
| `f6_55` | Row: 9a. Buildings and other depreciable assets. Column: Beginning of tax year. (b). |
| `f6_56` | Row: 9a. Buildings and other depreciable assets. Column: End of tax year. (c). |
| `f6_57` | Row: 9a. Buildings and other depreciable assets. Column: End of tax year. (d). |
| `f6_58` | Row: 9b. Less accumulated depreciation. Column: Beginning of tax year. (a). |
| `f6_59` | Row: 9b. Less accumulated depreciation. Column: Beginning of tax year. (b). |
| `f6_60` | Row: 9b. Less accumulated depreciation. Column: End of tax year. (c). |
| `f6_61` | Row: 9b. Less accumulated depreciation. Column: End of tax year. (d). |
| `f6_62` | Row: 10a. Depletable assets. Column: Beginning of tax year. (a). |
| `f6_63` | Row: 10a. Depletable assets. Column: Beginning of tax year. (b). |
| `f6_64` | Row: 10a. Depletable assets. End of tax year. (c). |
| `f6_65` | Row: 10a. Depletable assets. End of tax year. (d). |
| `f6_66` | Row: 10b. Less accumulated depletion. Column: Beginning of tax year. (a). |
| `f6_67` | Row: 10b. Less accumulated depletion. Column: Beginning of tax year. (b). |
| `f6_68` | Row: 10b. Less accumulated depletion. Column: End of tax year. (c). |
| `f6_69` | Row: 10b. Less accumulated depletion. Column: End of tax year. (d). |
| `f6_70` | Row: 11. Land (net of any amortization). Column: Beginning of tax year. (a). |
| `f6_71` | Row: 11. Land (net of any amortization). Column: Beginning of tax year. (b). |
| `f6_72` | Row: 11. Land (net of any amortization). Column: End of tax year. (c). |
| `f6_73` | Row: 11. Land (net of any amortization). Column: End of tax year. (d). |
| `f6_74` | Row: 12a. Intangible assets (amortizable only). Column: Beginning of tax year. (a). |
| `f6_75` | Row: 12a. Intangible assets (amortizable only). Column: Beginning of tax year. (b). |
| `f6_76` | Row: 12a. Intangible assets (amortizable only). Column: End of tax year. (c). |
| `f6_77` | Row: 12a. Intangible assets (amortizable only). Column: End of tax year. (d). |
| `f6_78` | Row: 12b. Less accumulated amortization. Column: Beginning of tax year. (a). |
| `f6_79` | Row: 12b. Less accumulated amortization. Column: Beginning of tax year. (b). |
| `f6_80` | Row: 12b. Less accumulated amortization. Column: End of tax year. (c). |
| `f6_81` | Row: 12b. Less accumulated amortization. Column: End of tax year. (d). |
| `f6_82` | Row: 13. Other assets (attach statement). Column: Beginning of tax year. (a). |
| `f6_83` | Row: 13. Other assets (attach statement). Column: Beginning of tax year. (b). |
| `f6_84` | Row: 13. Other assets (attach statement). Column: End of tax year. (c). |
| `f6_85` | Row: 13. Other assets (attach statement). Column: End of tax year. (d). |
| `f6_86` | Row: 14. Total assets. Column: Beginning of tax year. (a). |
| `f6_87` | Row: 14. Total assets. Column: Beginning of tax year. (b). |
| `f6_88` | Row: 14. Total assets. Column: End of tax year. (c). |
| `f6_89` | Row: 14. Total assets. Column: End of tax year. (d). |
| `f6_90` | Liabilities and Capital. Row: 15. Accounts payable. Column: Beginning of tax year. (a). |
| `f6_91` | Row: 15. Accounts payable. Column: Beginning of tax year. (b). |
| `f6_92` | Row: 15. Accounts payable. Column: End of tax year. (c). |
| `f6_93` | Row: 15. Accounts payable. Column: End of tax year. (d). |
| `f6_94` | Row: 16. Mortgages, notes, bonds payable in less than 1 year. Column: Beginning of tax year. (a). |
| `f6_95` | Row: 16. Mortgages, notes, bonds payable in less than 1 year. Column: Beginning of tax year. (b). |
| `f6_96` | Row: 16. Mortgages, notes, bonds payable in less than 1 year. Column: End of tax year. (c). |
| `f6_97` | Row: 16. Mortgages, notes, bonds payable in less than 1 year. Column: End of tax year. (d). |
| `f6_98` | Row: 17. Other current liabilities (attach statement). Column: Beginning of tax year. (a). |
| `f6_99` | Row: 17. Other current liabilities (attach statement). Column: Beginning of tax year. (b). |
| `f6_100` | Row: 17. Other current liabilities (attach statement). Column: End of tax year. (c). |
| `f6_101` | Row: 17. Other current liabilities (attach statement). Column: End of tax year. (d). |
| `f6_102` | Row: 18. All nonrecourse loans. Column: Beginning of tax year. (a). |
| `f6_103` | Row: 18. All nonrecourse loans. Column: Beginning of tax year. (b). |
| `f6_104` | Row: 18. All nonrecourse loans. Column: End of tax year. (c). |
| `f6_105` | Row: 18. All nonrecourse loans. Column: End of tax year. (d). |
| `f6_106` | Row: 19a. Loans from partners (or persons related to partners). Column: Beginning of tax year. (a). |
| `f6_107` | Row: 19a. Loans from partners (or persons related to partners). Column: Beginning of tax year. (b). |
| `f6_108` | Row: 19a. Loans from partners (or persons related to partners). Column: End of tax year. (c). |
| `f6_109` | Row: 19a. Loans from partners (or persons related to partners). Column: End of tax year. (d). |
| `f6_110` | Row: 19b. Mortgages, notes, bonds payable in 1 year or more. Column: Beginning of tax year. (a). |
| `f6_111` | Row: 19b. Mortgages, notes, bonds payable in 1 year or more. Column: Beginning of tax year. (b). |
| `f6_112` | Row: 19b. Mortgages, notes, bonds payable in 1 year or more. Column: End of tax year. (c). |
| `f6_113` | Row: 19b. Mortgages, notes, bonds payable in 1 year or more. Column: End of tax year. (d). |
| `f6_114` | Row: 20. Other liabilities (attach statement). Column: Beginning of tax year. (a). |
| `f6_115` | Row: 20. Other liabilities (attach statement). Column: Beginning of tax year. (b). |
| `f6_116` | Row: 20. Other liabilities (attach statement). Column: End of tax year. (c). |
| `f6_117` | Row: 20. Other liabilities (attach statement). Column: End of tax year. (d). |
| `f6_118` | Row: 21. Partners’ capital accounts. Column: Beginning of tax year. (a). |
| `f6_119` | Row: 21. Partners' capital accounts. Column: Beginning of tax year. (b). |
| `f6_120` | Row: 21. Partners’ capital accounts. Column: End of tax year. (c). |
| `f6_121` | Row: 21. Partners’ capital accounts. Column: End of tax year. (d). |
| `f6_122` | Row: 22. Total liabilities and capital. Column: Beginning of tax year. (a). |
| `f6_123` | Row: 22. Total liabilities and capital. Column: Beginning of tax year. (b). |
| `f6_124` | Row: 22. Total liabilities and capital. Column: End of tax year. (c). |
| `f6_125` | Row: 22. Total liabilities and capital. Column: End of tax year. (d). |
| `f6_126` | Schedule M-1. Reconciliation of Income (Loss) per Books With Analysis of Net Income (Loss) per Return. Note: The partnership may be required to file Schedule M-3. See instructions. 1. Net income (loss) per books. |
| `f6_127` | 2. Income included on Schedule K, lines 1, 2, 3c, 5, 6a, 7, 8, 9a, 10, and 11, not recorded on books this year (itemize): |
| `f6_128` | 2. Amount. |
| `f6_129` | 3. Guaranteed payments (other than health insurance). |
| `f6_130` | 4. Expenses recorded on books this year not included on Schedule K, lines 1 through 13e, and 21 (itemize): a. Depreciation. $. |
| `f6_131` | 4b. Travel and entertainment. $. |
| `f6_132` | 4b. Amount. |
| `f6_133` | 5. Add lines 1 through 4. |
| `f6_134` | 6. Income recorded on books this year not included on Schedule K, lines 1 through 11 (itemize): a. Tax-exempt interest. $. Line 1 of 2. |
| `f6_135` | 6a. Line 2 of 2. |
| `f6_136` | 6a. Amount. |
| `f6_137` | 7. Deductions included on Schedule K, lines 1 through 13e, and 21, not charged against book income this year (itemize): a. Depreciation. $. Line 1 of 2. |
| `f6_138` | 7a. Line 2 of 2. |
| `f6_139` | 7a. Amount. |
| `f6_140` | 8. Add lines 6 and 7. |
| `f6_141` | Income (loss) (Analysis of Net Income (Loss) per Return, line 1). Subtract line 8 from line 5. |
| `f6_142` | Schedule M-2. Analysis of Partners' Capital Accounts. 1. Balance at beginning of year. |
| `f6_143` | 2. Capital contributed: a. Cash. |
| `f6_144` | 2b. Property. |
| `f6_145` | 3. Net income (loss) (see instructions). |
| `f6_146` | 4. Other increases (itemize): |
| `f6_147` | 4. Amount. |
| `f6_148` | 5. Add lines 1 through 4. |
| `f6_149` | 6. Distributions. a. Cash. |
| `f6_150` | 6b. Property. |
| `f6_151` | 7. Other decreases (itemize): Line 1 of 2. |
| `f6_152` | 7. Line 2 of 2. |
| `f6_153` | 7. Amount. |
| `f6_154` | 8. Add lines 6 and 7. |
| `f6_155` | 9. Balance at end of year. Subtract line 8 from line 5. |

## Schedule K-1 (f1065sk1.pdf) — 111 fields

| Field | Description |
| --- | --- |
| `f1_1` | Page 1. For calendar year 2025, or tax year beginning. 2 digit month. |
| `f1_2` | 2 digit day. |
| `f1_3` | 2025 ending. 2 digit month. |
| `f1_4` | 2 digit day. |
| `f1_5` | 4 digit year. |
| `c1_1` | Final K-1. |
| `c1_2` | Amended K-1. |
| `f1_6` | Part I. Information About the Partnership. A. Partnership's employer identification number. |
| `f1_7` | B. Partnership's name, address, city, state, and Z I P code. |
| `f1_8` | C. I R S center where partnership filed return: |
| `c1_3` | D. Check if this is a publicly traded partnership (P T P). |
| `f1_9` | Part I I. Information About the Partner. E. Partner's S S N or T I N (Do not use T I N of a disregarded entity. See instructions.). |
| `f1_10` | F. Name, address, city, state, and Z I P code for partner entered in E. See instructions. |
| `c1_4` | G. General partner or L L C member-manager. |
| `c1_4` | G. Limited partner or other L L C member. |
| `c1_5` | H1. Domestic partner. |
| `c1_5` | H1. Foreign partner. |
| `c1_6` | H2. If the partner is a disregarded entity (D E), enter the partner's: |
| `f1_11` | H2. T I N. |
| `f1_12` | H2. Name. |
| `f1_13` | I1. What type of entity is this partner? |
| `c1_7` | I2. If this partner is a retirement plan (I R A/S E P/ Keogh/etc.), check here. |
| `f1_14` | J. Partner's share of profit, loss, and capital (see instructions): Row: Profit. Column: Beginning. %. |
| `f1_15` | Row: Profit. Column: Ending. %. |
| `f1_16` | Row: Loss. Column: Beginning. %. |
| `f1_17` | Row: Loss. Column: Ending. %. |
| `f1_18` | Row: Capital. Column: Beginning. %. |
| `f1_19` | Row: Capital. Column: Ending. %. |
| `c1_8` | J. Check if decrease is due to: Sale. |
| `c1_8` | J. or Exchange of partnership interest. See instructions. |
| `f1_20` | K1. Partner’s share of liabilities. Row: Nonrecourse. Column: Beginning. $. |
| `f1_21` | Row: Nonrecourse. Column: Ending. $. |
| `f1_22` | Row: Qualified nonrecourse financing. Column: Beginning. $. |
| `f1_23` | Row: Qualified nonrecourse financing. Column: Ending. $. |
| `f1_24` | Row: Recourse. Column: Beginning. $. |
| `f1_25` | Row: Recourse. Column: Ending. $. |
| `c1_9` | K2. Check this box if item K1 includes liability amounts from lower-tier partnerships. |
| `c1_10` | K3. Check if any of the above liability is subject to guarantees or other payment obligations by the partner. See instructions. |
| `f1_26` | L. Partner's capital account analysis: Row: Beginning capital account. $. |
| `f1_27` | L. Row: Capital contributed during the year. $. |
| `f1_28` | L. Current year net income (loss). $. |
| `f1_29` | L. Row: Other increase (decrease) (attach explanation). $. |
| `f1_30` | L. Row: Withdrawals and distributions. Column: Open parenthesis. Close parenthesis. $. |
| `f1_31` | L. Row: Ending capital account. $. |
| `c1_11` | M. Did the partner contribute property with a built-in gain (loss)? If "Yes," attach statement. See instructions. Yes. |
| `c1_11` | M. No. |
| `f1_32` | N. Partner’s Share of Net Unrecognized Section 704(c) Gain or (Loss). Beginning. $. |
| `f1_33` | N. Ending. $. |
| `f1_34` | Part I I I Partner’s Share of Current Year Income, Deductions, Credits, and Other Items 1. Ordinary business income (loss). |
| `f1_35` | 2. Net rental real estate income (loss). |
| `f1_36` | 3. Other net rental income (loss). |
| `f1_37` | 4a. Guaranteed payments for services. |
| `f1_38` | 4b. Guaranteed payments for capital. |
| `f1_39` | 4c. Total guaranteed payments. |
| `f1_40` | 5. Interest income. |
| `f1_41` | 6a. Ordinary dividends. |
| `f1_42` | 6b. Qualified dividends. |
| `f1_43` | 6c. Dividend equivalents. |
| `f1_44` | 7. Royalties. |
| `f1_45` | 8. Net short-term capital gain (loss). |
| `f1_46` | 9a. Net long-term capital gain (loss). |
| `f1_47` | 9b. Collectibles (28%) gain (loss). |
| `f1_48` | 9c. Unrecaptured section 1250 gain. |
| `f1_49` | 10. Net section 1231 gain (loss). |
| `f1_50` | 11. Other income (loss). Code. Line 1 of 2. |
| `f1_51` | 11. Amount. Line 1 of 2. |
| `f1_52` | 11. Code. Line 2 of 2. |
| `f1_53` | 11. Amount. Line 2 of 2. |
| `f1_54` | 12. Section 179 deduction. |
| `Line13` | 13. Other deductions. Code. Line 1 of 3. |
| `f1_55` | 13. Amount. Line 1 of 3. |
| `f1_56` | 13. Code. Line 2 of 3. |
| `f1_57` | 13. Amount. Line 2 of 3. |
| `f1_58` | 13. Code. Line 3 of 3. |
| `f1_59` | 13. Amount. Line 3 of 3. |
| `Line14` | 14. Self-employment earnings (loss). Code. Line 1 of 2. |
| `f1_60` | 14. Amount. Line 1 of 2. |
| `f1_61` | 14. Code. Line 2 of 2. |
| `f1_62` | 14. Amount. Line 2 of 2. |
| `Line15` | 15. Credits. Code. Line 1 of 2. |
| `f1_63` | 15. Amount. Line 1 of 2. |
| `f1_64` | 15. Code. Line 2 of 2. |
| `f1_65` | 15. Amount. Line 2 of 2. |
| `c1_12` | 16. Schedule K-3 is attached if checked. |
| `Line17` | 17. Alternative minimum tax (A M T) items. Code. Line 1 of 3. |
| `f1_79` | 17. Amount. Line 1 of 3. |
| `f1_80` | 17. Code. Line 2 of 3. |
| `f1_81` | 17. Amount. Line 2 of 3. |
| `f1_82` | 17. Code. Line 3 of 3. |
| `f1_83` | 17. Amount. Line 3 of 3. |
| `Line18` | 18. Tax-exempt income and nondeductible expenses. Code. Line 1 of 3. |
| `f1_84` | 18. Amount. Line 1 of 3. |
| `f1_85` | 18. Code. Line 2 of 3. |
| `f1_86` | 18. Amount. Line 2 of 3. |
| `f1_87` | 18. Code. Line 3 of 3. |
| `f1_88` | 18. Amount. Line 3 of 3. |
| `Line19` | 19. Distributions. Code. Line 1 of 2. |
| `f1_89` | 19. Amount. Line 1 of 2. |
| `f1_90` | 19. Code. Line 2 of 2. |
| `f1_91` | 19. Amount. Line 2 of 2. |
| `Line20` | 20. Other information. Code. Line 1 of 4. |
| `f1_92` | 20. Amount. Line 1 of 4. |
| `f1_93` | 20. Code. Line 2 of 4. |
| `f1_94` | 20. Amount. Line 2 of 4. |
| `f1_95` | 20. Code. Line 3 of 4. |
| `f1_96` | 20. Amount. Line 3 of 4. |
| `f1_97` | 20. Code. Line 4 of 4. |
| `f1_98` | 20. Amount. Line 4 of 4. |
| `f1_66` | 21. Foreign taxes paid or accrued. |
| `c1_13` | 22. More than one activity for at-risk purposes*. *See attached statement for additional information. |
| `c1_14` | 23. More than one activity for passive activity purposes*. *See attached statement for additional information. |
