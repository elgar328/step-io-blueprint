# Third-party schema notice

The `*.exp` files in this directory are **EXPRESS schemas defined by ISO 10303
(STEP)**. They are included as input to the tool and are **not** covered by this
repository's MIT license — they retain their original terms.

The EXPRESS schemas (ISO 10303-11) are published free for implementers, distinct
from the copyrighted ISO 10303 standard documents. The authoritative free source
is the ISO **SMRL** (STEP Module & Resource Library),
`standards.iso.org/iso/10303/smrl/v<N>/tech/smrlv<N>.zip`. Schemas the SMRL does
not carry (the legacy non-modular APs) come from MBx-IF / STEPCode. Upstream files
ship with CRLF line endings; here they are normalized to LF (content otherwise
unchanged).

Files are named `ap<part>e<edition>.exp` (the edition/revision details live here,
not in the filename). All are **long-form** implementation schemas (the model a
Part 21 `.stp` file conforms to) — `mim_lf` for the modular APs, `aim` (its
monolithic-era equivalent) for AP203 Ed 1 and AP214.

| file | edition | model | entities | source |
|---|---|---|---|---|
| `ap203e1.exp` | AP203 Ed 1 (1994, IS) | AIM | 254 | STEPCode [^legacy] |
| `ap203e2.exp` | AP203 Ed 2 (2011, IS) | MIM | 1006 | **SMRL v12** |
| `ap214e3.exp` | AP214 Ed 3 (2010, IS) | AIM | 915 | MBx-IF [^legacy] |
| `ap242e1.exp` | AP242 Ed 1 (2014, IS) | MIM | 1726 | MBx-IF [^ed1] |
| `ap242e2.exp` | AP242 Ed 2 (2020, IS) | MIM | 2122 | **SMRL v8** |
| `ap242e3.exp` | AP242 Ed 3 (2022, IS, withdrawn) | MIM | 2140 | **SMRL v9** |
| `ap242e4.exp` | AP242 Ed 4 (2025, IS) | MIM | 2407 | **SMRL v12** / ISO/TS 10303-442 ed 7 [^ed4] |

**SMRL version → AP242 edition:** v8 = Ed 2 (2122), v9 = Ed 3 (2140), v10 = Ed 3
rev (2145), v11 = a revision between Ed 3 and Ed 4 (N11273, 2392), v12 = Ed 4
(N11521, 2407, 2025). AP203 Ed 2 is stable at 1006
across all versions. The SMRL contains only **modular** APs — it has **no** AP214
and **no** legacy AP203 `config_control_design`.

**Publication tiers:** the AP242 *standard document* (ISO 10303-242) is an
International Standard in every edition: Ed 1 (2014), Ed 2 (2020), Ed 3 (2022,
since withdrawn) and Ed 4 (2025). Its EXPRESS *schema files* are distributed
with the module Technical Specification ISO/TS 10303-442.

[^ed4]: Also published on its own, byte for byte the same, at
`standards.iso.org/iso/ts/10303/-442/ed-7/tech/express/mim_lf.exp`. This file is
that download unchanged (it already has LF line endings).

[^legacy]: Legacy non-modular AP (AIM long form), absent from the SMRL, so sourced
from its original distributor. AP214 is superseded by AP242.

[^ed1]: The SMRL carries no 2014 Ed 1 IS — AP242 only enters the SMRL at v6 (a
stub) and v7 (a 2018 development snapshot, 1981 entities), both later than and
distinct from the 2014 Ed 1. This file is the published Ed 1 IS (N8324, 1726
entities), archived by MBx-IF. It is redundant in the union (Ed 2 fully covers
Ed 1) but kept to preserve the genuine Ed 1 baseline and the `ap242e1` label that
the codegen/tests reference.

If you redistribute these files, keep this notice and observe the terms of the
respective upstream sources.
