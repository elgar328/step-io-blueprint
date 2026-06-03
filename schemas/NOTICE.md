# Third-party schema notice

The `*.exp` files in this directory are **EXPRESS schemas defined by ISO 10303
(STEP)**. They are included unmodified as input to the tool and are **not**
covered by this repository's MIT license — they retain their original terms.

The EXPRESS schemas (ISO 10303-11) are published for free use by implementers
(ISO hosts them at `standards.iso.org`; STEPCode and the MBx-IF redistribute
them), and are distinct from the copyrighted ISO 10303 standard documents.

| file | schema | source |
|---|---|---|
| `ap203.exp` | AP203 (Configuration Controlled 3D Design) | [STEPCode](https://github.com/stepcode/stepcode) `data/ap203/` (BSD-3-Clause) — this edition is not offered by MBx-IF |
| `ap203e2_mim_lf.exp` | AP203 Edition 2 (2011) | [MBx-IF](https://www.mbx-if.org/home/mbx/resources/express-schemas/) — distributed as `part403ts_wg3n2635mim_lf.exp` (byte-identical) |
| `ap214e3.exp` | AP214 Edition 3 (2010) | MBx-IF — `AP214E3_2010.exp` (byte-identical) |
| `ap242_mim_lf.exp` | AP242 Edition 1 (2014) | STEPCode `data/ap242/` — MBx-IF's IS longform (`ap242_is_mim_lf_v1.36`) differs, so STEPCode's copy is kept |
| `ap242ed2_dis2_mim_lf_v1.101.exp` | AP242 Edition 2 (2019, N10517) | MBx-IF |
| `ap242ed3_mim_lf_v1.152.exp` | AP242 Edition 3 (2022) | MBx-IF |

If you redistribute these files, keep this notice and observe the terms of the
respective upstream sources.
