# infer 파이프라인 — 미흡 부분 (정밀화 대상)

전체 3 stage end-to-end 동작은 확인됨 (commit `3cfa522` ~ `0ead7f2`).
첫 cut 분포는 다음과 같음:

| Stage | confident | review | unresolved | 입력 | 출력 |
|---|---|---|---|---|---|
| variant | 825 | 828 | 127 | 1,780 entity (4 schema union) | 1,780 결정 |
| arena | 362 | 0 | 0 | 825 confident variant | 362 group |
| pool | 362 | 0 | 0 | 362 arena | 94 pool (connected component) |

variant 단순화 후 (commit `ac2dbf8`) 의 분포:

| Stage | output | 비고 |
|---|---|---|
| variant | 1,780 (single=349 enum=1362 nested=69) | bucket/pending 폐기, 모두 자동 결정 |
| arena | 421 group | strict gate 자연 통과 |
| pool | (미측정) | — |

SUPERTYPE 절 파서 + 분류기 정밀화 후 분포:

| Stage | output | 비고 |
|---|---|---|
| variant | 1,780 (single=239 enum=1244 nested=12 enum_base=173 merged_into=97 complex=13 composite=2) | unresolved=0, parse_warnings=0 (4 schema 모두) |
| arena | 436 group | confident=436 review=0 unresolved=0 |
| pool | 436 group → 97 distinct pool | confident=436 review=0 unresolved=0 |

ConcreteSupertype 자동 분류 후 분포 (§8 처리됨):

| Stage | output | 비고 |
|---|---|---|
| variant | 1,780 (single=195 enum=1223 nested=2 enum_base=173 merged_into=97 complex=13 composite=2 concrete_super=75) | 이전 silent SingleStruct/InEnum/NestedField 75 건이 정확한 자리로 |
| arena | 462 group | confident=462 review=0 unresolved=0 |
| pool | 462 group → 93 distinct pool | confident=462 review=0 unresolved=0 |

분포 변동의 본질: 이전 단순 분류기가 silent fallback / 누락된 mixin 으로
부정확하게 single / enum 으로 분류하던 entity 들이 정확한 자리로 이동
(enum_base / merged_into / complex_supertype / composite_one_of 가 명시
변형으로 분리됨).

53k STEP 파일 가지치기 후 분포 (Plan 3b 처리됨):

| Stage | output | 비고 |
|---|---|---|
| usage | 1,780 entities (used=296 unused=1,484) | corpus 미등장 entity 가 압도 — geometry 편향 corpus |
| variants_pruned | 240 entities | transitive cascade 후 (75 ConcreteSupertype 중 18 → SingleStruct, 44 → 제거) |
| arenas_pruned | 130 groups | variants_pruned 기준 재계산. 원본 462 → 130 |

원본 variants.toml / arenas.toml / pools.toml 은 *불변*. 가지치기는 *별
view*. corpus 변동 시 prune 만 재실행. 실행 시간 ~85 초 (1,780 alternation
regex + 53k 파일 ~2.6GB 처리, single-thread).

*가지치기 산출이 매우 좁음* (240 entities) — 현재 corpus 의 geometry
편향 반영. PMI / property / metadata 도메인 entity 가 corpus 미등장.
fixtures 확장으로 산출 entity 수 증가 — 운영 영역 (plan scope X).

본 문서는 정밀화 단계에서 다뤄야 할 미흡 부분을 항목별로 정리. 임시 문서로,
정밀화 후 폐기 또는 INFRA_PLAN.md 같은 영구 문서로 이관 예정.

## 1. SUPERTYPE 절 파싱 정밀도 — 처리됨

**상황**: 이전 regex + paren-depth 기반 ad-hoc 파서가 `ONEOF (...) AND
ONEOF (...)` 의 `AND` 키워드를 인식 못 했고, `(ref ANDOR ONEOF (...))` 의
leading bare ref 를 누락하고, `ONEOF (..., (loop ANDOR path))` 같은 ONEOF
멤버 안의 합성 노드를 평탄화로 silent 오분류했다. 4 unique entity 가
silent 깨짐 + 14 unique 패턴이 부정확.

**해결**: EXPRESS § 9.2.4 그대로의 recursive descent 파서 + 트리 기반
`SupertypeExpr` IR + `VariantSpec::ComplexSupertype` 의 mixin 필드를 raw
SupertypeExpr 로 + B7 자동 분류 (`VariantSpec::CompositeOneOf` + `Rule
1.5`) + Rule 8 unresolved safety net.

**검증**:
- 4 schema 351 SUPERTYPE 절 모두 0 parse_warnings 로 통과
- 14 unique 패턴 모두 자동 분류 (B0-B7)
- 4 silent fail entity 의 정확한 새 분류 결과 단언으로 박힘
  (`infer::variant::tests::silent_fail_entities_classify_correctly_on_real_schemas`)
- `variants_pending.toml` 미생성 (= 1,780 entity 모두 confident)

**잔존 영역**:
- 미래 schema 진화로 Rule 1 / 1.5 가 못 잡는 더 깊은 anonymous 합성 패턴
  → Rule 8 으로 surface → `variants_pending.toml` 생성 → 사용자가
  `variants_overrides.toml` 에 명시 결정 → 도구 재실행 → unresolved 비어
  짐 → 자동 삭제. 인프라만 준비.

## 3. arena 단계의 ID 분리 신호 미구현

**현재**: 1 group = 1 arena 디폴트. 모든 group 이 confident=0.9 ~ 0.95.

**미흡한 점**: plan 의 arena confidence 신호 3 개 중 ID 분리 (가중치 0.5) 가 미적
용. 실제로는:
- step-io 의 IR 가 같은 EXPRESS type 인 `cartesian_point` 를 두 arena 로 분리
  (`points` + `points_2d`) 한 이유는 PCURVE 의 definitional context 와 일반 3D
  context 를 ID 타입으로 구분하기 위함.
- 현재 분류기는 이 분리를 못 함 → step-io 와 mismatch.

**구현 방향**:
- representation_context 추출: `representation` ATTR 의 type 이 어느 context 를
  받는지 분석. `definitional_representation` 의 후손 안에서 출현 vs 일반
  `geometric_representation_context` 안에서 출현.
- 같은 group 의 entity 들이 두 context 모두에서 등장하면 → 같은 group 을 두
  arena 로 분리 (`<group>` + `<group>_2d` 같은 명명).
- Confidence: clean 분리면 1.0, mixed 면 review.

**난이도**: schema 의 representation_context 의미적 분석이 필요. 단순 ATTR ref
graph 에서 한 단계 더 들어감. plan 의 약 100~200 lines 추가 예상.

## 4. pool 단계 — 자동 분류 폐기, 100% 수동 + strict gate (Plan 3e ✓)

**진단 (커밋 da76ba9 시점)**: 가지치기 후 130 arena → 32 pool, 그러나 87
arena 가 한 거대 pool 로 묶임. cross-reference 풍부한 schema 에서 connected
component 가 *의미축* (geometry / topology / pmi / etc.) 을 못 잡음.
Louvain / modularity community detection 도 *의미축* 을 자동으로 알아낼
정보 X — *사용자 도메인 mental model* 이 본질.

**해결 (Plan 3e)**: 자동 분류 통째 폐기. shape stage 패턴 복제 (수동
입력 + strict gate). 사용자가 `pools.toml` 에 130 arena 마다 pool 직접
명시. 도구는 missing → Err, extra → warning 의 검증만.

**근거**:
- pool 결정 = 사용자 mental model 영역 (자동의 한계 명확).
- shape stage 의 13 건 결정과 같은 패턴 — *판단 영역의 결정은 입력 파일,
  도구는 검증* 의 책임 분리.
- 자동 후보가 부정확하면 *사용자가 검토 안 함* — 경계 케이스에서 잘못
  분류되어 IR 사용성 파괴 가능. 수동 강제로 회피.

## 6. 그 외 작은 정리

- `infer arena|pool` 의 `--allow-pending` 같은 옵션이 main.rs 의 단일 flag 로
  처리됨. variant stage 도 같은 파이프라인 안에 있을 때 일관성 있는 옵션 처리
  체계 필요. (variant 단순화 후 variant 영역에선 무관해졌고, arena/pool 영역만
  남음.)
- pending.toml 의 sort order 가 BTreeMap 기반이라 deterministic 하지만, review
  / unresolved 표시 순서가 entity 알파벳순임. confidence 낮은 순으로 정렬하면
  사용자가 "가장 의심스러운 것부터" 검토 가능. (arena/pool 만 해당.)
- override 파일의 stale 검증 메시지에 schema source 정보 추가 (어느 schema 에서
  사라졌는지) — `--diff` 모드와 결합 시 유용. (arena/pool 만 해당.)

## 7. arena 단계 — 거대 enum 의 sub-group 분리 (variant 단순화 부수 효과)

**현재**: variant 단순화 (commit `ac2dbf8`) 후 자동 분류 결과에서 50+ 멤버
enum 6 개 검출:

| enum_root | 멤버 수 |
|---|---|
| data_quality_criterion | 120 |
| generic_expression | 82 |
| geometric_representation_item | 81 |
| shape_aspect | 70 |
| representation_item | 52 |
| characterized_object | 51 |

**미흡한 점**:
- variant 단계는 schema 의 inheritance 를 정직하게 옮기는 것이 책임. 거대
  supertype (`representation_item` 등) 까지 polymorphic context 에 등장하면
  자연스레 거대 enum 으로 분류됨.
- step-io IR 측에서는 이 거대 enum 이 너무 광범위 → 도메인별로 좁은 enum 으로
  분리하는 게 적정.
- §3 (ID 분리) 와 다른 차원의 분리 신호 — context 가 아니라 도메인 (geometry /
  topology / pmi / measure 등) 기반 클러스터링.

**구현 방향**:
- 같은 group 안의 entity 들 사이의 ATTR ref 그래프를 보고, 도메인 sub-cluster
  가 분리되면 group 도 분리.
- 또는 step-io IR 디자이너가 명시적 override 로 supertype 별 sub-group 매핑.
- arena 단계의 explicit override (`[group.X] arena = ...`) 메커니즘이 이미 있음
  → 단지 sub-group 분리 룰만 추가하면 됨.

**난이도**: §3 (ID 분리) 와 함께 다루면 효율적. 분리 신호 모두 arena 단계의
group → arena 매핑 정밀화에 속함.

## 8. ABSTRACT/ONEOF 미명시 concrete supertype — 처리됨

**처리 방식**: schema 의 4 신호 (SUPERTYPE OF 절 부재 + own_attrs ≥ 1 +
직접 자식 ≥ 1 + polymorphic_targets 등장) 를 만족하는 entity 를 자동
`ConcreteSupertype` 으로 분류. pass2 의 *override 직후 / Rule 5 앞* 에
배치되어 chain 케이스 (자기 위에 또 다른 implicit supertype 이 있는
entity) 도 동일 패턴으로 처리.

**최종 카운트**: **75 건** (이전 §8 추정 24 건 + 정확 진단으로 발견된 50
건). 이전 진단의 24 건 추정은 *proxy polymorphic_targets* (variants.toml
의 InEnum.enum_name 집합) 기준이었고, 실 분류기의 ATTR cross-reference
graph 기준 polymorphic_targets 가 더 넓은 집합. 50 건은 이전에 silent 로
SingleStruct / InEnum / NestedField 잘못 분류되던 entity 들. 자동 룰이
이걸 모두 정확하게 ConcreteSupertype 으로 잡음.

**arena/pool 영향**:
- arena: 436 group → 462 group (+26). 이전 충돌로 누락되던 group 들 정상화
- pool: distinct pools 97 → 93 (-4 — pool 통합)

**53k STEP 파일 사용 통계**: 24 건 sample 의 카테고리 분포 (구현 전 진단):
- only_parent (자식 0): 5 건 (e.g. general_property 568 self / 0 children)
- mixed_parent_dominant (child/self < 5): 3 건
- mixed_children_dominant (child/self ≥ 5): 2 건
- both_zero (이 corpus 미등장, 다른 도메인): 14 건

→ 24 건 안에서도 사용 패턴 극단적 차이. 일괄 IR 강제 부담.

**IR shape 결정의 책임**: `ConcreteSupertype` 라벨은 *schema 사실* 만 박음.
IR 의 Rust 코드 모양 (Carrier enum / base struct + parallel enum / 단독
struct) 결정은 **step-io 측 lowering 의 책임**. 입력으로:
- 이 분류표 (variants.toml 의 `concrete_supertype` 라벨)
- 53k STEP 파일 사용 통계 (case-by-case 가지치기)
- SUBTYPE OF graph (chain hierarchy 복원 — chain entity 는 부모 enum 의
  자식 명단에서 빠지므로 graph 직접 활용 필요)

**chain hierarchy 부분 손실**: chain entity (예: representation_relationship_with_transformation)
가 자기 sub-enum 의 base 로 분류되면서 그 부모 enum (representation_relationship)
의 직접 자식 명단에서 빠짐. 의도된 변동. SUBTYPE OF graph 는 refgraph 에
보존되므로 step-io lowering 이 graph 직접 활용으로 복원 가능.

## 우선순위 (제안)

전체 작업 흐름은 IR_DESIGN.md 의 *IR Roadmap plan 의 작업 항목* 섹션과
align. schema-check 측 단계만 이곳에서 추적.

```
Plan 1 ✓ — SUPERTYPE 절 파서 정확도 (commit 6a19d83)
Plan 2 ✓ — ConcreteSupertype 자동 분류 (commit 310eaa1)
Plan 3a — arena 보수적 자동 분류 (skip)
Plan 3b ✓ — 53k STEP 파일 통계 가지치기
Plan 3c ✓ — ConcreteSupertype IR shape 결정
Plan 3d — *제거됨* (Lossy 정책 → round-trip 테스트 의미적 정확화로 책임 이관)
Plan 3e ✓ — pool 분류 (수동 입력 + strict gate, shape 패턴 복제)
Plan 3f ✓ — IR 친화 명명 + ir.toml 청사진 산출 (entities + pools + names + schemas 통합)
Plan 3.7 ✓ — arena 의 3-bucket 인프라 부분 폐기
Plan 3.8 ✓ (Phase 1) — infer reshape stage: split / merge 추상화 통합 자리 (infrastructure)
Plan 3.9 ✓ — splits/merges 의 reasons 필드 + per-variant kind override
Plan 3.10 ✓ — 2D NURBS 짝 (NurbsCurve2d / NurbsSurface2d) + face_surface BaseParallel
Plan 3.11 ✓ — Merge target 의 kind / enum_of override (split 인프라 대칭)
Plan 3.12 Phase 1 ✓ — prune_overrides infra (ABSTRACT supertype keep)
Plan 3.12 Phase 2 ✓ — recasts infra (1→1 grouped reclassification in reshape)
Plan 3.13 ✓ — infer prune 의 SquashFS corpus 지원 (backhand in-process streaming)
Plan 3.12 Phase 3 ✓ — Curve / Surface enum 통합 (3D, keep+recast 적극 blueprint)
Plan 3.15 ✓ — pools.toml 의 unused arena entry cleanup (Phase 3 후속, 5 dangling)
Plan 3.16 ✓ — reshape 의 빈 enum_base 자동 cleanup (Phase 3 후속, 4 dangling)
Plan 3.17 ✓ — degenerate_toroidal_surface recast 추가 + elementary_surface cleanup
Plan 3.18 ✓ — 1-child enum_base collapse + dangling refs 정책 Err 통일 (Plan 3.16 안전망 갱신)
Plan 3.19 Phase 1 ✓ — anchors.toml 인프라 (4번째 추상화: anchor). 빈 입력 → ir.toml diff 0.
Plan 3.19 Phase 2a ✓ — ParameterSpaceCurve anchor (anchors.toml 첫 실제 사용, pcurve / bounded_pcurve InEnum).
Plan 3.19 Phase 2b ✓ — SurfaceTraceCurve naming + 멤버 복원 (schema 구조 존중, intersection_curve / bounded_surface_curve keep).
```

각 plan 의 책임:

- ~~**3a (arena 자동)**~~ — **skip 결정**. 진단 결과 자동 룰의 효과가
  사실상 0 또는 부작용 위험: Rule B (root disjoint 강제 분리) 발동 후보
  0 (variants 가 이미 root SUPERTYPE 따라 정렬됨), Rule A (SELECT 멤버
  통합) 후보 253 건 중 대부분이 *generic cross-cutting SELECT* (예:
  approved_item 9 distinct groups) — 무차별 통합 시 거대 generic enum
  발생 → IR 사용성 파괴. arena 단계는 *1 group = 1 arena default + arenas_overrides
  적용* 유지. 거대 group 분할은 Plan 3b 가 통계로 흡수.
  **Plan 3.7 (commit 후속) 으로** Decision wrap / 3-bucket / batch_accept
  scaffolding 폐기 — auto 룰이 항상 confident 0.9~0.95 이라 review/unresolved
  자리 0 의 dead code 였음. arenas.toml / arenas_pruned.toml 의 source/
  confidence/reasons noise 제거. variant 의 Rule 8 unresolved 안전망
  (Decision/PendingFile/Unresolved) 은 보존.
- **3b (가지치기) ✓** — `infer prune --corpus <path>` sub-command. 외부
  53k STEP 파일 corpus 경로 인자로 받음 (fixtures 복사 X). instance
  카운트 측정 + P-2 transitive 가지치기 + arena 재계산. 산출 3 파일:
  `usage.toml` (모든 entity 카운트), `variants_pruned.toml`,
  `arenas_pruned.toml`. 원본 variants/arenas/pools 불변. 실 corpus
  결과: 1,780 → 296 used / 1,484 unused, variants_pruned 240 entities,
  arenas_pruned 130 groups, ~85 초 소요. 거대 group cluster 분할
  (co-occurrence 기반) 은 *후속 plan*
- **3c (ConcreteSupertype shape) ✓** — `infer shape` sub-command. 두
  책임: (1) 가지치기 후 살아남은 ConcreteSupertype (현재 13 건) 각각의
  IR shape 결정 검증 (*수동 입력* `inferred/shapes.toml` vs required
  set; missing → Err, extra → warning); (2) 검증 통과 후 *통합 view*
  `entities.toml` 산출 — variants_pruned + arenas_pruned + shapes +
  usage 를 entity 단위 단일 표로 응축. pool (Plan 3e) / 명명 (Plan 3f)
  의 *단일 입력*. shape 자동화는 ratio 단일 신호로 가능했으나 (Carrier
  측 ratio ≥ 1.99 vs Base+Parallel 측 ≤ 0.020 의 100 배 gap) ratio 가
  못 잡는 신호 (children 의 attr 구조, 도메인 mental model) 와 *경계
  케이스 사람 검토 강제* 위해 의도적으로 수동 선택. 결과: Carrier 8 +
  Base+Parallel 5 = 13. fixtures 확장으로 ConcreteSupertype 늘면 strict
  gate 가 누락분 잡음 → 사용자가 추가 entry 만 수동 작성
- ~~**3d (Lossy)**~~ — **제거 결정**. 본래 의도 (어느 attr 가 typed
  field, 어느 attr 가 round-trip default) 의 동기는 *부정확한 round-trip
  테스트 우회용 텍스트 보존*. 텍스트 보존은 IR 의 책임이 아니고 *테스트
  설계의 약점*. 모든 attr 는 *typed* (필요 시 default 값 제공). round-trip
  비교의 정확성 (의미적 동등성, ISO 의무 placeholder 무시 등) 은 step-io
  측 round-trip 테스트의 책임 → Phase 2.2 / 운영 영역으로 이관. lossy
  marker / lossy_overrides 산출 X. schema-check 측 결정 면적 축소
- **3e (pool) ✓** — `infer pool` sub-command. 100% 수동 입력 + strict
  gate (shape 패턴 복제). 사용자가 `pools.toml` 직접 작성 (arena 별 1
  entry: `[arena.X] pool = "Y"`); 도구는 `arenas_pruned.toml` 의 130
  required arena 와 비교 검증 — missing → Err, extra → warning. 산출 파일
  X (입력이 곧 step-io codegen 입력). 자동 분류는 폐기 (cross-ref 풍부
  schema 에서 union-find 의 거대 component 1 개 수렴 — INFER_TUNING §4 의
  진단)
- **3f (이름) ✓** — `infer naming` sub-command. *분류 파이프라인의 마지막
  layer*. type / id / variant / enum / kind_enum / field 의 IR 친화 이름
  결정. **자동 default** (snake → PascalCase / `<type>Id` / attr 그대로 /
  Carrier 의 `<type>Data` / Base+Parallel 의 `<type>Kind`) + **사용자
  partial override** (`names.toml` — 카테고리별 flat, 빈 파일 OK, 누락 X).
  entities + pools + names + schemas 통합 → `ir.toml` (entity 단위 단일
  IR 청사진) 산출. *codegen 의 단일 입력*. attr type 까지 prefix string
  으로 변환 (예: `list_ref_X` / `opt_real`) + inherited attrs 펼침 + TYPE
  alias resolution. stale 검사는 warning (Err X) — 사용자 typo / 가지치기
  변동 자연 흡수

**책임 분리 원칙**: 모든 분류 / 사람 결정 / 통계 가지치기가 본 도구
(schema-check) 측. step-io 측은 schema-check 의 최종 산출만 받아 *기계적*
IR 코드 생성. step-io 자기완결성을 위해 schema-check 의 외부 fixtures
의존 (53k corpus 경로 인자) 은 OK — 외부 도구 간 상호 의존 허용.

**기존 INFER_TUNING.md § 항목과 새 plan 의 매핑**:
- ~~§8~~ (concrete supertype) — Plan 2 ✓
- §3 (arena ID 분리) + §7 (거대 enum sub-group) → Plan 3a + 3b 흡수
- §4 (pool community detection) → Plan 3e
- §6 (작은 정리) → 우선순위 낮음, 후속

## 검증 방향

정밀화 작업의 검증 게이트:
- arena: 일부 group 이 ID 분리로 두 arena 로 나뉘는 결과 확인 (예:
  `points` + `points_2d`). 거대 enum (50+ 멤버 6 개) 도 sub-group 으로 분해
  되어 enum 당 멤버 수 분포가 평탄화 (p95 < 30 정도 목표).
- variant supertype 처리: `surface` / `representation_item` 등 supertype entity
  의 분류가 일관 (둘 다 in_enum 또는 둘 다 single_struct, 또는 새 variant).
  arena 의 group 충돌 0 건.
- pool: 거대 단일 component 가 sub-cluster 로 분해되어 pool 수가 ~15~20 정도로
  안정. step-io 의 현재 `GeometryPool` / `TopologyPool` 등과 정합 비교.

각 단계의 `--idempotency` 검증, override 작성 후 cycle 검증 (override → 재실행
→ pending 단조 감소) 도 정밀화 후 한 번 더 수행. (variant 단계는 override
폐기 후 재실행마다 schema 결과로 deterministic.)
