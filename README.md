# step-io-blueprint

A schema-analysis tool that turns the EXPRESS schemas behind STEP into an
**IR blueprint** for [step-io](../step-io), a STEP/EXPRESS reader-writer.

step-io implements its intermediate representation (IR) by hand, one entity at
a time. This tool decides *what shape each entity should take* in that IR —
plain struct, enum variant, dispatch root, merged field, and so on — and emits
a single reference file, `inferred/ir.toml`, that step-io is written against.

The tool itself performs only **mechanical classification and validation**.
Every human decision lives in a small set of hand-authored files under
`inferred/`; otherwise the pipeline is deterministic and reproducible from the
schemas. There is no code generation — `ir.toml` is a reference humans read, not
source that is emitted.

## Quick start

```sh
cargo run --release -- <stage>     # run one pipeline stage; no arg prints usage
cargo test                         # unit tests
```

## Pipeline

Seven stages run in order. Each takes the previous stage's output, plus an
optional override file wherever a human decision is needed. The flow is strictly
one-directional: a downstream stage never mutates an upstream output.

```
variant → arena → prune → shape → reshape → pool → naming
```

| Stage | What it does | Hand-authored input | Output |
|---|---|---|---|
| `variant` | Classify every entity into one of 8 IR shapes from its SUPERTYPE/SUBTYPE structure + attributes | `variants_overrides.toml` | `variants.toml` |
| `arena`   | Group entities that share an enum; map group → arena | `arenas_overrides.toml` | `arenas.toml` |
| `prune`   | Drop entities absent from the corpus (transitive cascade) using frozen instance counts | `prune_overrides.toml`, `corpus_usage.toml` | `variants_pruned.toml`, `arenas_pruned.toml`, `usage.toml` |
| `shape`   | Validate the IR shape (carrier vs base+parallel) of each surviving supertype; condense to one view | `shapes.toml` | `entities.toml` |
| `reshape` | Apply abstraction decisions: split, merge, recast, anchor | `splits.toml`, `merges.toml`, `recasts.toml`, `anchors.toml` | `abstract_entities.toml` |
| `pool`    | Validate the arena → pool (module) assignment | `pools.toml` | (validation only) |
| `naming`  | Pick IR-friendly type/id/variant/enum/field names; merge everything | `names.toml` | **`ir.toml`** |

`ir.toml` is the deliverable: a per-entity blueprint that step-io is
implemented against.

### The 8 IR shapes

`variant` assigns each entity exactly one of: `SingleStruct`, `InEnum`,
`EnumBase` (struct-less dispatch root), `ConcreteSupertype` (struct + dispatch),
`MergedInto`, `NestedField`, `ComplexSupertype`, `CompositeOneOf`. The schema
alone cannot always tell a directly-instantiated supertype from a struct-less
dispatch root (`group` vs `edge`, say) — the corpus instance count settles that
in `prune`.

### corpus_usage.toml — no live corpus dependency

`prune` never scans a STEP corpus. Per-entity instance counts
(`instance_count`, `complex_part_count`, `co_instantiated_with`) are frozen in
`inferred/corpus_usage.toml` and committed to this repo, so the whole pipeline
runs with **no external dependency**.

That file is produced by a separate corpus-scanning tool that walks a STEP-file
corpus, and is regenerated and copied in only when the corpus itself changes
(rare). Since it lists every entity name seen in the corpus, adding a schema
does not require regenerating it.

### Pending gate

Each stage refuses to run while an upstream `*_pending.toml` exists — the
`variant` stage writes one only when it meets a schema shape it cannot classify,
which has not happened yet. Pass `--allow-pending` to bypass during development.

## Schemas

Six schemas are read as a union — five from
[MBx-IF](https://www.mbx-if.org/home/mbx/resources/express-schemas/), and only
the original AP203 (ed1), which MBx-IF does not offer, from
[STEPCode](https://github.com/stepcode/stepcode). Line endings are normalized
to LF (MBx-IF ships CRLF); content is unchanged.

| schema | source |
|---|---|
| `ap203.exp` | STEPCode — AP203 ed1 |
| `ap203e2_mim_lf.exp` | MBx-IF — AP203 ed2 |
| `ap214e3.exp` | MBx-IF — AP214 ed3 |
| `ap242_mim_lf.exp` | MBx-IF — AP242 ed1 |
| `ap242ed2_dis2_mim_lf_v1.101.exp` | MBx-IF — AP242 ed2 |
| `ap242ed3_mim_lf_v1.152.exp` | MBx-IF — AP242 ed3 |

Only mechanical-CAD schemas are used; the AP209/210/238/239/240, IFC,
ISO 15926, and PDM domains are out of scope.

These `.exp` files are third-party ISO 10303 (STEP) schemas, not covered by
this repo's license — see [`schemas/NOTICE.md`](schemas/NOTICE.md).

## Layout

```
src/
├── main.rs              CLI dispatch
├── express.rs           EXPRESS schema parser (.exp → Schema, incl. SUPERTYPE clauses)
└── infer/
    ├── refgraph.rs      ATTR cross-reference graph
    ├── variant.rs       1 — classify each entity into an IR shape
    ├── arena.rs         2 — group → arena mapping
    ├── prune.rs         3 — drop unused entities via corpus_usage.toml
    ├── shape.rs         4 — validate supertype shapes → entities.toml
    ├── reshape.rs       5 — split / merge / recast / anchor → abstract_entities.toml
    ├── pool.rs          6 — validate arena → pool assignment
    └── naming.rs        7 — name + merge everything → ir.toml
schemas/                 the six .exp schema files
inferred/                hand-authored inputs + generated outputs
```

`inferred/` holds two kinds of files: **hand-authored inputs** (the
`*_overrides.toml`, `shapes.toml`, `pools.toml`, `names.toml`, `splits.toml`,
`merges.toml`, `recasts.toml`, `anchors.toml`, and the frozen
`corpus_usage.toml`) and **generated outputs** (`variants.toml`, `arenas.toml`,
`variants_pruned.toml`, `usage.toml`, `entities.toml`,
`abstract_entities.toml`, `ir.toml`, …). Both are committed so the blueprint is
browsable without running the tool; only transient `*_pending.toml` gate files
are ignored.

## License

Source code is MIT-licensed ([`LICENSE`](LICENSE)). The EXPRESS schemas under
`schemas/` are third-party ISO 10303 artifacts with their own terms — see
[`schemas/NOTICE.md`](schemas/NOTICE.md).
