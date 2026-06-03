# step-io-blueprint

EXPRESS schema 분석 도구 — 4 STEP schema (AP203 / AP203e2 / AP214e3 /
AP242) 를 union 으로 읽고, *step-io 측 IR 코드를 수작업으로 작성할 때
참조하는 청사진* 을 산출한다. 사람 결정은 모두 `inferred/` 의 사용자
입력 파일에 누적, 도구는 *기계적 변환 + 검증* 만 수행. step-io 측의 IR
구현은 entity 별 수작업 — 본 도구는 *어떤 모양으로 만들지의 reference*
만 제공.

## 사용

```
cargo run --release -- <stage>    # 분류 / 검증 / 응축 파이프라인
```

`<stage>` 는 아래 파이프라인의 한 단계. 인자 없이 실행하면 usage 출력.

## 파이프라인

6 stage 를 순서대로 실행. 각 stage 는 *upstream stage 의 산출* 을 입력으로
받아, 사람 결정 (overrides) 이 필요한 자리에서만 사용자 입력 파일을 추가
참조. **stage 간 단방향 흐름** — downstream 이 upstream 의 산출을
변경하지 않음.

```
variant → arena → prune → shape → reshape → pool → naming
```

`naming` 은 분류 파이프라인의 *마지막 layer* — type / id / variant /
enum / field 의 IR 친화 이름 결정 + 모든 stage 산출 (entities + pools +
names + schemas) 통합 → step-io 측 수작업 구현의 *단일 reference* `ir.toml` 산출.
사용자는 `names.toml` 의 *어색한 자리만* override (자동 default 가
대부분 OK).

### 입출력 표

| Stage | 사용자 입력 (`inferred/`) | 도구 입력 | 외부 의존 | 산출 |
|---|---|---|---|---|
| `variant` | `variants_overrides.toml` (선택) | `schemas/*.exp` | — | `variants.toml`, (`variants_pending.toml`) |
| `arena` | `arenas_overrides.toml` (선택) | `variants.toml`, `schemas/*.exp` | — | `arenas.toml` |
| `prune` | `prune_overrides.toml` (선택, ABSTRACT supertype keep), `inferred/corpus_usage.toml` (frozen, vendored) | `variants.toml`, `arenas_overrides.toml` | — (외부 corpus 직접 접근 없음) | `usage.toml`, `variants_pruned.toml`, `arenas_pruned.toml` |
| `shape` | `shapes.toml` (수동, ConcreteSupertype 별 1 entry) | `variants_pruned.toml`, `arenas_pruned.toml`, `usage.toml` | — | (검증 + 통과 시 `entities.toml` 자동 응축) |
| `reshape` | `splits.toml` + `merges.toml` + `recasts.toml` + `anchors.toml` (수동, 빈 파일 OK) | `entities.toml` | — | `abstract_entities.toml` (split / merge / recast / anchor 적용 후 view) |
| `pool` | `pools.toml` (수동, arena 별 1 entry) | `abstract_entities.toml` | — | (검증만; missing → Err, extra → warning) |
| `naming` | `names.toml` (수동 partial — 어색한 자리만 override) | `entities.toml`, `pools.toml`, `schemas/*.exp` | — | `ir.toml` (entity 단위 단일 IR 청사진 — step-io 측 수작업 구현의 reference) |

`variants_pending.toml` 은 variant stage 가 *Rule 8 unresolved 안전망* 으로
*예상 외 schema 모양* 을 발견했을 때만 생성 — 파일 존재 자체가 "다음
stage 진입 차단" 의 strict gate 신호 (현재까지 한 번도 생성된 적 없음).
`*_overrides.toml` 은 사용자가 *직접 작성* 하는 결정 파일 (자동 분류의
재해석 / 좁은 사람 개입). `shapes.toml` 은 *override 가 아닌 수동 입력*
(ConcreteSupertype 의 IR shape — 자동 분류 X).

### corpus 의존 없음 — `inferred/corpus_usage.toml` (frozen, vendored)

`prune` 은 STEP corpus 를 직접 스캔하지 않는다. 코퍼스 인스턴스 카운트는
`inferred/corpus_usage.toml` (등장 entity 별 instance_count / complex_part_count /
co_instantiated_with) 에 프로즌으로 담겨 repo 에 커밋돼 있고, `prune` 은 이를 읽어
자기 entity 집합으로 필터한다. 따라서 청사진 파이프라인은 **외부 의존이 전혀 없다**.

`corpus_usage.toml` 은 코퍼스를 소유한 외부 도구 **step-io-reference-check** 의
`corpus-usage` bin 이 생성한다 (`*.sqfs` 컨테이너를 backhand 로 스트리밍 스캔).
코퍼스가 바뀔 때만 (드물게) 재생성:

```
# step-io-reference-check 에서:
cargo run --release --bin corpus-usage      # → corpus_usage.toml
# 그 파일을 청사진으로 복사 후 커밋:
cp corpus_usage.toml <blueprint>/inferred/corpus_usage.toml
```

스키마를 추가해도 그 entity 가 코퍼스에 있으면 이미 요약에 들어 있어 (요약은 코퍼스
전체 entity 명을 담음) 재생성이 불필요하다 — 재생성은 코퍼스 변동 시에만.

### Stage 책임 요약

- **`variant`** — 6 schema 의 모든 entity (2,195) 를 8 가지 IR shape
  (SingleStruct / InEnum / EnumBase / MergedInto / NestedField /
  ComplexSupertype / CompositeOneOf / ConcreteSupertype) 로 자동 분류.
  결정 신호는 SUPERTYPE / SUBTYPE 구조 + ATTR.
- **`arena`** — variants 분류를 group (같은 enum 으로 묶이는 entity
  들의 묶음) 으로 변환하고 group → arena 매핑. **1 group = 1 arena 가
  default** (arena 이름 = group 이름) + `arenas_overrides.toml` 로
  사용자가 *그루핑 / 이름* 변경 가능. 자동 룰이 trivial 이라 별도
  3-bucket 처리 없음.
- **`prune`** — `inferred/corpus_usage.toml` (frozen) 의 instance 카운트로
  *현재 사용되지 않는* entity 를 식별 + transitive cascade 로 흡수된
  entity 도 정리. 산출은 *별 view* — 원본 variants/arenas 는 불변.
  `prune_overrides.toml` 의 `[keep.X]` 항목으로 *ABSTRACT supertype* (예:
  curve / surface — corpus instance 0 이지만 IR polymorphism root 로 필요)
  을 *수동 보존*.
- **`shape`** — 가지치기 후 살아남은 ConcreteSupertype (현재 13 건)
  각각의 IR shape (Carrier vs Base+Parallel) 결정 검증 + 4 입력을
  *entity 단위 단일 view* (`entities.toml`) 로 응축. reshape stage 의
  단일 입력.
- **`reshape`** — *추상화 결정의 단일 자리*. 네 추상화 유형:
  split (1 schema entity → N IR type, 예: cartesian_point → Point3 +
  Point2), merge (N schema entity → 1 IR type, 예: NurbsCurve 가
  b_spline_* 류 흡수), recast (1 schema entity → 1 IR type 의
  *grouped reclassification*, 예: line / circle / ... 을 일괄적으로
  Curve enum 의 InEnum variant 로), anchor (0 → 1: schema 에 없는 IR
  EnumBase anchor 를 추가해 후속 recasts 의 `enum_of` target 으로 사용).
  `abstract_entities.toml` 산출. 빈 입력 시 entities.toml 그대로 복제 —
  점진 도입. 각 entry 는 `reasons` 로 *왜 이 추상화가 schema 1:1 보다
  나은 IR 디자인인지* 의 근거를 보존 (ir.toml 의 primary entity 에
  propagate). split / merge / recast 모두 `kind` / `enum_of` override 로
  target VariantSpec 조정 가능 — split 측은 per-variant, merge 측은
  per-target, recast 측은 per-group (entries 배열의 모든 entity 가 동일
  target VariantSpec 공유). 적용 순서: splits → merges → anchors →
  recasts (recast 는 post-abstraction state 위에서 동작). step-io 의
  추상화 결정이 *코드 marker* 가 아닌 *splits.toml / merges.toml /
  recasts.toml / anchors.toml* 에 모임.
- **`pool`** — arena → pool (코드 폴더 / sub-crate) 묶음. shape 와
  같은 *수동 입력 + strict gate* 패턴 — `pools.toml` 사용자 직접 작성,
  도구는 검증만 (missing → Err, extra → warning). 자동 분류는 효과 0
  (cross-ref 풍부 schema 에서 union-find 가 거대 component 1 개로 수렴)
  으로 폐기.
- **`naming`** — 분류 파이프라인의 *마지막 layer*. type / id /
  variant / enum / field 의 IR 친화 이름 결정. 자동 default (snake_case →
  PascalCase, `<type>Id`, attr 그대로) + 사용자 점진 override
  (`names.toml` partial — 빈 파일 OK). entities + pools + names +
  schemas 의 attr type 까지 통합한 *entity 단위 단일 청사진* `ir.toml`
  산출 — step-io 측에서 entity 를 *수작업으로 한 명씩 추가할 때 참조*
  하는 단일 파일 (codegen 미사용). 알려진 약어 (B-spline / NURBS)
  자동 인식 X (사용자 override 영역).

### Pending gate

각 stage 는 upstream pending 파일 존재 시 진입 차단:

```bash
$ cargo run --release -- arena
arena failed:
variants_pending.toml exists — variant stage has unresolved/review items.
Resolve in variants_overrides.toml or pass --allow-pending.
```

`--allow-pending` 플래그로 우회 가능 (개발 / 진단 시).

## Schema 출처

`schemas/` 의 6 파일 — 4 개는 [STEPCode](https://github.com/stepcode/stepcode)
의 `data/` 에서, AP242 e2/e3 2 개는 MBx-IF (CAx-IF) 에서:

| schema | 출처 |
|---|---|
| ap203.exp | `stepcode/data/ap203/ap203.exp` |
| ap203e2_mim_lf.exp | `stepcode/data/ap203e2/ap203e2_mim_lf.exp` |
| ap214e3.exp | `stepcode/data/ap214e3/AP214E3_2010.exp` |
| ap242_mim_lf.exp | `stepcode/data/ap242/242_n8324_mim_lf.exp` |
| ap242ed2_dis2_mim_lf_v1.101.exp | MBx-IF (AP242 ed2, 2019/N10517) |
| ap242ed3_mim_lf_v1.152.exp | MBx-IF (AP242 ed3, 2022) |

step-io 의 mechanical CAD 도메인에 해당하는 6 schema 만 사용 (AP242 ed2/ed3
포함). AP209 / AP210 / AP238 / AP239 / AP240 / IFC / ISO15926 / pdm 등 다른
도메인 schema 는 제외.

Schema 갱신 시 (STEPCode 새 release):
```sh
cp ~/Desktop/references/stepcode/data/ap203/ap203.exp schemas/
cp ~/Desktop/references/stepcode/data/ap203e2/ap203e2_mim_lf.exp schemas/
cp ~/Desktop/references/stepcode/data/ap214e3/AP214E3_2010.exp schemas/ap214e3.exp
cp ~/Desktop/references/stepcode/data/ap242/242_n8324_mim_lf.exp schemas/ap242_mim_lf.exp
# 이후 infer 파이프라인 재실행 (variant → arena → prune → shape → pool → naming)
```

## Architecture

```
src/
├── main.rs                    CLI dispatch (<stage>)
├── express.rs / express/      EXPRESS schema parser (.exp → Schema, SUPERTYPE 절 포함)
└── infer/
    ├── mod.rs                 공유 type (Decision / Bucket / InferResult)
    ├── io.rs                  inferred/*.toml read/write + pending gate
    ├── overrides.rs           overrides 파일 loader + 검증
    ├── refgraph.rs            ATTR cross-reference graph (arena 자동 분류 입력)
    ├── variant.rs             stage 1 — entity 의 IR shape 자동 분류
    ├── arena.rs               stage 2 — group → arena 매핑 + entity → group 인덱스
    ├── prune.rs               stage 3 — corpus_usage.toml 읽어 가지치기 + transitive cascade
    ├── shape.rs               stage 4 — ConcreteSupertype shape 검증 + entities.toml 응축
    ├── reshape.rs             stage 5 — split / merge / recast 추상화 적용 → abstract_entities.toml
    ├── pool.rs                stage 6 — pools.toml 검증 (수동 입력 vs abstract_entities 의 required arena set)
    └── naming.rs              stage 7 — auto default + names.toml overrides → ir.toml 청사진
schemas/                       4 STEP schema (.exp 파일)
inferred/                      stage 산출 + 사용자 입력 파일 (대부분 gitignored)
```

`inferred/` 의 추적 정책 (gitignore):
- *사용자 입력 파일* (`*_overrides.toml`, `shapes.toml`, `pools.toml`,
  `names.toml`, `splits.toml`, `merges.toml`, `recasts.toml`,
  `anchors.toml`) — tracked
- *도구 산출 파일* (`variants.toml`, `arenas.toml`,
  `variants_pruned.toml`, `arenas_pruned.toml`, `usage.toml`,
  `entities.toml`, `abstract_entities.toml`, `ir.toml`, `*_pending.toml`)
  — gitignored (재실행으로 복원 가능)

*entities.toml vs abstract_entities.toml*: 두 view 보존. entities.toml =
shape 의 schema 1:1 분류 응축. abstract_entities.toml = reshape 의 split /
merge / recast 적용 후. step-io 측 reference 는 ir.toml 단일 — 두 중간 view 는
디버깅 / 추상화 결정 검증 용도.

## Tests

```
cargo test
```

EXPRESS parser + variants 자동 분류 + arena group 매핑 + prune transitive
cascade + shape 검증 / entities 응축 의 단위 테스트.
